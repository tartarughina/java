#!/usr/bin/env python3
"""
Generate a JSON Schema for JDTLS initialization options from Preferences.java.

This script parses the Preferences.java source file from eclipse.jdt.ls to extract
all preference keys, their types, default values, and documentation comments, then
produces a JSON Schema (Draft 7) that validates the `initializationOptions.settings.java`
object sent during the LSP initialize request.

Usage:
    # Fetch the latest Preferences.java and generate the schema:
    python generate-schema.py

    # Use a local copy of Preferences.java:
    python generate-schema.py --input /path/to/Preferences.java

    # Specify output path:
    python generate-schema.py --output ../jdtls-initialization-options.schema.json

The generated schema covers:
  - Top-level InitializationOptions (bundles, workspaceFolders, settings)
  - settings.java.* preferences as a flat map that gets nested into a JSON structure

Source of truth:
  https://github.com/eclipse-jdtls/eclipse.jdt.ls/blob/main/org.eclipse.jdt.ls.core/src/org/eclipse/jdt/ls/core/internal/preferences/Preferences.java
"""

import argparse
import json
import re
import sys
import urllib.request
from collections import OrderedDict
from typing import Any, Optional

PREFERENCES_URL = (
    "https://raw.githubusercontent.com/eclipse-jdtls/eclipse.jdt.ls/"
    "main/org.eclipse.jdt.ls.core/src/org/eclipse/jdt/ls/core/internal/preferences/Preferences.java"
)

# ---------------------------------------------------------------------------
# Step 1: Parse constant declarations from Preferences.java
# ---------------------------------------------------------------------------

# Matches:  public static final String SOME_KEY = "java.foo.bar";
CONST_RE = re.compile(
    r'public\s+static\s+final\s+String\s+(\w+)\s*=\s*"(java\.[^"]+)"\s*;'
)

# Matches Javadoc or line comments immediately preceding a constant
JAVADOC_RE = re.compile(r"/\*\*(.+?)\*/", re.DOTALL)
LINE_COMMENT_RE = re.compile(r"//\s*(.*)")

# Matches default value assignments in the static initializer or constructor
# e.g.  JAVA_IMPORT_EXCLUSIONS_DEFAULT.add("**/node_modules/**");
DEFAULT_ADD_RE = re.compile(
    r'(\w+_DEFAULT)\s*\.\s*add\(\s*"([^"]+)"\s*\)'
)

# Matches simple default constant assignments
# e.g.  public static final int JAVA_COMPLETION_MAX_RESULTS_DEFAULT = 50;
DEFAULT_INT_RE = re.compile(
    r"public\s+static\s+final\s+int\s+(\w+_DEFAULT)\s*=\s*(\d+)\s*;"
)

# Matches field defaults in constructor like:  implementationsCodeLens = "none";
CONSTRUCTOR_DEFAULT_STR_RE = re.compile(
    r'(\w+)\s*=\s*"([^"]+)"\s*;'
)
CONSTRUCTOR_DEFAULT_BOOL_RE = re.compile(
    r"(\w+)\s*=\s*(true|false)\s*;"
)
CONSTRUCTOR_DEFAULT_INT_RE = re.compile(
    r"(\w+)\s*=\s*(\d+)\s*;"
)
CONSTRUCTOR_DEFAULT_ENUM_RE = re.compile(
    r"(\w+)\s*=\s*(\w+)\.(\w+)\s*;"
)


def fetch_preferences_java(url: str) -> str:
    """Download Preferences.java from GitHub."""
    print(f"Fetching {url} ...", file=sys.stderr)
    req = urllib.request.Request(url)
    with urllib.request.urlopen(req, timeout=30) as resp:
        return resp.read().decode("utf-8")


def extract_javadoc_before(source: str, match_start: int) -> Optional[str]:
    """Extract Javadoc or line comment immediately before a position."""
    preceding = source[:match_start].rstrip()

    # Try Javadoc block
    if preceding.endswith("*/"):
        jd_start = preceding.rfind("/**")
        if jd_start != -1:
            raw = preceding[jd_start + 3 : -2]
            lines = []
            for line in raw.split("\n"):
                line = line.strip().lstrip("*").strip()
                if line:
                    lines.append(line)
            return " ".join(lines)

    # Try single-line comment(s)
    lines_before = preceding.split("\n")
    comment_lines = []
    for line in reversed(lines_before):
        stripped = line.strip()
        if stripped.startswith("//"):
            comment_lines.insert(0, stripped[2:].strip())
        else:
            break
    if comment_lines:
        return " ".join(comment_lines)

    return None


# Descriptions for intermediate grouping objects (non-leaf nodes) in the schema.
# These nodes don't correspond to a single preference key — they are structural
# containers that group related preferences. The JSON language server shows these
# when hovering over the group key (e.g. "signatureHelp", "completion").
INTERMEDIATE_NODE_DESCRIPTIONS = {
    "autobuild": "Auto build settings.",
    "cleanup": "Code cleanup action settings.",
    "codeAction": "Code action settings.",
    "codeGeneration": "Code generation settings (hashCode/equals, toString, comments, etc.).",
    "compile": "Compilation settings (null analysis, etc.).",
    "completion": "Code completion settings (favorites, import order, postfix, chain, etc.).",
    "configuration": "Project configuration settings (runtimes, build configuration, Maven).",
    "contentProvider": "Content provider settings (preferred decompiler, etc.).",
    "diagnostic": "Diagnostic filtering settings.",
    "eclipse": "Eclipse-specific settings (download sources, etc.).",
    "edit": "Editor behavior settings (smart semicolon, buffer validation, etc.).",
    "executeCommand": "Execute command settings.",
    "foldingRange": "Code folding settings.",
    "format": "Code formatter settings (enable/disable, profile, on-type, comments, etc.).",
    "hover": "Hover settings (Javadoc, etc.).",
    "import": "Project import settings (Gradle, Maven, exclusions).",
    "imports": "Import settings (Gradle wrapper checksums).",
    "inlayHints": "Inlay hints settings (parameter names, variable types, etc.).",
    "jdt": "JDT-specific settings (language server features).",
    "maven": "Maven settings (download sources, update snapshots).",
    "project": "Project settings (encoding, output path, source paths, referenced libraries, resource filters).",
    "quickfix": "Quick fix settings.",
    "refactoring": "Refactoring settings (extract interface, etc.).",
    "references": "References settings (include accessors, decompiled sources).",
    "referencesCodeLens": "References code lens settings.",
    "rename": "Rename settings.",
    "saveActions": "Save action settings (organize imports, cleanup).",
    "search": "Search settings (scope).",
    "selectionRange": "Selection range settings.",
    "settings": "External settings (URL to settings file).",
    "signatureHelp": "Signature help settings (enable/disable, API descriptions).",
    "sources": "Source settings (organize imports thresholds).",
    "symbols": "Symbol settings (include source method declarations, generated code).",
    "telemetry": "Telemetry settings.",
    "templates": "Code templates (file header, type comment).",
    "updateImportsOnPaste": "Settings for organizing imports on paste.",
    # Nested intermediates
    "codeGeneration.hashCodeEquals": "Settings for hashCode/equals generation.",
    "codeGeneration.toString": "Settings for toString() generation.",
    "compile.nullAnalysis": "Annotation-based null analysis settings.",
    "completion.chain": "Chain completion settings.",
    "completion.postfix": "Postfix completion settings.",
    "completion.lazyResolveTextEdit": "Lazy resolve text edit settings for completion.",
    "configuration.maven": "Maven-specific project configuration.",
    "edit.smartSemicolonDetection": "Smart semicolon detection settings.",
    "format.comments": "Comment formatting settings.",
    "format.onType": "On-type formatting settings.",
    "format.settings": "External formatter settings (URL, profile).",
    "import.gradle": "Gradle import settings.",
    "import.gradle.annotationProcessing": "Gradle annotation processing settings.",
    "import.gradle.java": "Java home for Gradle builds.",
    "import.gradle.offline": "Gradle offline mode settings.",
    "import.gradle.user": "Gradle user home settings.",
    "import.gradle.wrapper": "Gradle wrapper settings.",
    "import.maven": "Maven import settings.",
    "import.maven.offline": "Maven offline mode settings.",
    "inlayHints.parameterNames": "Parameter name inlay hints settings.",
    "jdt.ls": "JDT Language Server settings (protobuf, Android, AspectJ, Kotlin, Groovy, javac).",
    "jdt.ls.androidSupport": "Android project support settings.",
    "jdt.ls.aspectjSupport": "AspectJ support settings.",
    "jdt.ls.groovySupport": "Groovy support settings.",
    "jdt.ls.javac": "Javac compilation backend settings.",
    "jdt.ls.kotlinSupport": "Kotlin support settings.",
    "jdt.ls.protobufSupport": "Protocol Buffers support settings.",
    "signatureHelp.description": "Signature help API description settings.",
    "sources.organizeImports": "Organize imports settings (star thresholds).",
    "refactoring.extract": "Extract refactoring settings.",
    "refactoring.extract.interface": "Extract interface refactoring settings.",
    "errors": "Error reporting settings.",
    "errors.incompleteClasspath": "Incomplete classpath error settings.",
    "codeAction.sortMembers": "Sort members code action settings.",
    "hover.javadoc": "Javadoc hover settings.",
    "imports.gradle": "Gradle import settings (wrapper checksums).",
    "imports.gradle.wrapper": "Gradle wrapper checksum settings.",
    "inlayHints.formatParameters": "Format parameters inlay hints settings.",
    "inlayHints.parameterTypes": "Parameter type inlay hints settings (e.g. lambda parameters).",
    "inlayHints.variableTypes": "Variable type inlay hints settings (e.g. var declarations).",
}


# Fallback descriptions for constants that have no Javadoc in Preferences.java.
# These are maintained by hand — see MAINTENANCE.md for guidance.
DESCRIPTION_OVERRIDES = {
    "JAVA_CLEANUPS_ACTIONS_ON_SAVE_DEPRECATED": (
        "Deprecated. List of cleanup action IDs to run on save. Use 'java.cleanup.actions' instead."
    ),
    "JAVA_CLEANUPS_ACTIONS_ON_SAVE_CLEANUP": (
        "Enable/disable running cleanup actions on save."
    ),
    "JAVA_CODEACTION_SORTMEMBER_AVOIDVOLATILECHANGES": (
        "Avoid reordering members that would cause volatile changes when sorting members."
    ),
    "JAVA_CODEGENERATION_TOSTRING_CODESTYLE": (
        "The code style to use when generating toString() methods. "
        "Values: STRING_CONCATENATION, STRING_BUILDER, STRING_BUILDER_CHAINED, STRING_FORMAT."
    ),
    "JAVA_CODEGENERATION_TOSTRING_LIMITELEMENTS": (
        "Limit the number of elements included in generated toString() output. 0 means no limit."
    ),
    "JAVA_CODEGENERATION_TOSTRING_LISTARRAYCONTENTS": (
        "Whether to list array contents in generated toString() methods."
    ),
    "JAVA_CODEGENERATION_TOSTRING_SKIPNULLVALUES": (
        "Whether to skip null values in generated toString() methods."
    ),
    "JAVA_COMPILE_NULLANALYSIS_MODE": (
        "Mode for annotation-based null analysis. "
        "'disabled' turns it off, 'interactive' prompts when annotations are detected, "
        "'automatic' enables it whenever null annotations are found on the classpath."
    ),
    "JAVA_COMPILE_NULLANALYSIS_NONNULL": (
        "Fully qualified names of @NonNull annotation types to use for null analysis."
    ),
    "JAVA_COMPILE_NULLANALYSIS_NONNULLBYDEFAULT": (
        "Fully qualified names of @NonNullByDefault annotation types to use for null analysis."
    ),
    "JAVA_COMPILE_NULLANALYSIS_NULLABLE": (
        "Fully qualified names of @Nullable annotation types to use for null analysis."
    ),
    "JAVA_COMPLETION_COLLAPSE_KEY": (
        "Whether to collapse overloaded completion items into a single entry."
    ),
    "JAVA_COMPLETION_FILTERED_TYPES_KEY": (
        "Types to filter (hide) from completion results. Supports wildcard patterns (e.g. 'com.sun.*')."
    ),
    "MAVEN_NOT_COVERED_PLUGIN_EXECUTION_SEVERITY": (
        "Severity of the problem marker for Maven plugin executions not covered by lifecycle mappings."
    ),
    "MAVEN_DEFAULT_MOJO_EXECUTION_ACTION": (
        "Default action for Maven mojo executions that are not covered by lifecycle mappings. "
        "Values: ignore, execute, warn, error."
    ),
    "JAVA_DIAGNOSTIC_FILER": (
        "Glob patterns to filter diagnostics from specific files or paths."
    ),
    "JAVA_EDIT_VALIDATE_ALL_OPEN_BUFFERS_ON_CHANGES": (
        "Whether to re-validate all open Java files when any file is changed, "
        "or only the changed file."
    ),
    "JAVA_INLAYHINTS_FORMATPARAMETERS_ENABLED": (
        "Enable/disable formatting for inlay hint parameters."
    ),
    "JAVA_INLAYHINTS_PARAMETERNAMES_SUPPRESS_WHEN_SAME_NAME_NUMBERED": (
        "Suppress parameter name inlay hints when the argument is a variable with the "
        "same name as the parameter, possibly followed by a number."
    ),
    "JAVA_INLAYHINTS_PARAMETERTYPES_ENABLED": (
        "Enable/disable inlay hints for parameter types in lambda expressions."
    ),
    "JAVA_INLAYHINTS_VARIABLETYPES_ENABLED": (
        "Enable/disable inlay hints for variable types (e.g. 'var' declarations)."
    ),
    "JAVA_JDT_LS_ANDROID_SUPPORT_ENABLED": (
        "Enable/disable Android project support in the language server."
    ),
    "JAVA_JDT_LS_ASPECTJ_SUPPORT_ENABLED": (
        "Enable/disable AspectJ (.aj) support in the language server."
    ),
    "JAVA_JDT_LS_GROOVY_SUPPORT_ENABLED": (
        "Enable/disable Groovy support in the language server."
    ),
    "JAVA_JDT_LS_JAVAC_ENABLED": (
        "Enable/disable using javac (OpenJDK compiler) instead of ECJ (Eclipse Compiler for Java) "
        "as the compilation backend."
    ),
    "JAVA_JDT_LS_KOTLIN_SUPPORT_ENABLED": (
        "Enable/disable Kotlin support in the language server."
    ),
    "JAVA_JDT_LS_PROTOBUF_SUPPORT_ENABLED": (
        "Enable/disable Protocol Buffers (protobuf) support in the language server."
    ),
    "JAVA_REFACTORING_EXTRACT_INTERFACE_REPLACE": (
        "Whether extracting an interface should replace all occurrences of the class "
        "with the new interface where possible."
    ),
    "JAVA_TELEMETRY_ENABLED_KEY": (
        "Enable/disable sending telemetry data to the language server."
    ),
}


def parse_constants(source: str) -> dict:
    """
    Parse all public static final String constants that map to java.* keys.
    Returns {constant_name: {"key": "java.foo.bar", "description": "..."}}
    """
    constants = OrderedDict()
    for m in CONST_RE.finditer(source):
        const_name = m.group(1)
        key = m.group(2)
        desc = extract_javadoc_before(source, m.start())
        if desc is None:
            desc = DESCRIPTION_OVERRIDES.get(const_name)
        constants[const_name] = {"key": key, "description": desc}
    return constants


def parse_list_defaults(source: str) -> dict:
    """Parse default list values from static initializer blocks."""
    defaults = {}
    for m in DEFAULT_ADD_RE.finditer(source):
        name = m.group(1)
        value = m.group(2)
        defaults.setdefault(name, []).append(value)
    return defaults


def parse_int_defaults(source: str) -> dict:
    """Parse default int constants."""
    defaults = {}
    for m in DEFAULT_INT_RE.finditer(source):
        name = m.group(1)
        value = int(m.group(2))
        defaults[name] = value
    return defaults


# ---------------------------------------------------------------------------
# Step 2: Determine type for each preference from updateFrom() method
# ---------------------------------------------------------------------------

# Patterns to detect type from the updateFrom method body
GETBOOLEAN_RE = re.compile(r"getBoolean\(\s*configuration\s*,\s*(\w+)")
GETSTRING_RE = re.compile(r"getString\(\s*configuration\s*,\s*(\w+)")
GETINT_RE = re.compile(r"getInt\(\s*configuration\s*,\s*(\w+)")
GETLIST_RE = re.compile(r"getList\(\s*configuration\s*,\s*(\w+)")
GETVALUE_RE = re.compile(r"getValue\(\s*configuration\s*,\s*(\w+)")


def determine_types(source: str) -> dict:
    """
    Determine the type of each constant by examining how it's read in updateFrom().
    Returns {constant_name: type_string} where type_string is one of:
      "boolean", "string", "integer", "string[]", "object", "special:..."
    """
    types = {}

    # Extract the updateFrom method body
    update_from_start = source.find("public static Preferences updateFrom(")
    if update_from_start == -1:
        print("WARNING: Could not find updateFrom method", file=sys.stderr)
        return types

    # Find the end of updateFrom - look for the next method at same indent level
    # We'll just grab a large chunk
    update_from_body = source[update_from_start:]

    for m in GETBOOLEAN_RE.finditer(update_from_body):
        types[m.group(1)] = "boolean"
    for m in GETSTRING_RE.finditer(update_from_body):
        const = m.group(1)
        if const not in types:
            types[const] = "string"
    for m in GETINT_RE.finditer(update_from_body):
        types[m.group(1)] = "integer"
    for m in GETLIST_RE.finditer(update_from_body):
        types[m.group(1)] = "string[]"
    for m in GETVALUE_RE.finditer(update_from_body):
        const = m.group(1)
        if const not in types:
            types[const] = "object"

    return types


# ---------------------------------------------------------------------------
# Step 3: Parse enum/mode types used in the source
# ---------------------------------------------------------------------------

def parse_enums(source: str) -> dict:
    """
    Parse Java enums and mode classes to determine allowed string values.
    Returns {constant_name: [allowed_values]} for constants that use enum-like types.
    """
    enums = {}

    # Map constant names to their known enum values based on source analysis.
    # These are derived from the fromString patterns and enum definitions in
    # Preferences.java and referenced classes.

    # FeatureStatus enum (used by updateBuildConfiguration and nullAnalysisMode)
    enums["CONFIGURATION_UPDATE_BUILD_CONFIGURATION_KEY"] = ["disabled", "interactive", "automatic"]
    enums["JAVA_COMPILE_NULLANALYSIS_MODE"] = ["disabled", "interactive", "automatic"]

    # implementationCodeLens is a string with known values
    enums["IMPLEMENTATIONS_CODE_LENS_KEY"] = ["none", "all", "types", "methods"]

    # quickfix.showAt
    enums["QUICK_FIX_SHOW_AT"] = ["line", "problem"]

    # InlayHintsParameterMode
    enums["JAVA_INLAYHINTS_PARAMETERNAMES_ENABLED"] = ["none", "literals", "all"]

    # ProjectEncodingMode
    enums["JAVA_PROJECT_ENCODING"] = ["ignore", "warning", "setdefault"]

    # CompletionMatchCaseMode
    enums["COMPLETION_MATCH_CASE_MODE_KEY"] = ["off", "firstletter"]

    # CompletionGuessMethodArgumentsMode (can also be boolean)
    enums["JAVA_COMPLETION_GUESS_METHOD_ARGUMENTS_KEY"] = [
        "off", "insertParameterNames", "insertBestGuessedArguments"
    ]

    # SearchScope
    enums["JAVA_SEARCH_SCOPE"] = ["all", "main"]

    # Severity-like strings (also used by errors.incompleteClasspath.severity in the wiki)
    enums["MAVEN_NOT_COVERED_PLUGIN_EXECUTION_SEVERITY"] = ["ignore", "info", "warning", "error"]
    enums["MAVEN_DEFAULT_MOJO_EXECUTION_ACTION"] = ["ignore", "execute", "warn", "error"]

    # codeGeneration.insertionLocation
    enums["JAVA_CODEGENERATION_INSERTIONLOCATION"] = ["lastMember", "beforeCursor"]

    # codeGeneration.addFinalForNewDeclaration
    enums["JAVA_CODEGENERATION_ADD_FINAL_FOR_NEW_DECLARATION"] = ["none", "all", "variables", "fields"]

    # toString.codeStyle
    enums["JAVA_CODEGENERATION_TOSTRING_CODESTYLE"] = [
        "STRING_CONCATENATION", "STRING_BUILDER", "STRING_BUILDER_CHAINED", "STRING_FORMAT"
    ]

    return enums


# Constants whose values may be null in practice (e.g. "preferred": null)
NULLABLE_CONSTANTS = {
    "PREFERRED_CONTENT_PROVIDER_KEY",
    "MAVEN_USER_SETTINGS_KEY",
    "MAVEN_GLOBAL_SETTINGS_KEY",
    "MAVEN_LIFECYCLE_MAPPINGS_KEY",
    "JAVA_HOME",
    "JAVA_FORMATTER_URL",
    "JAVA_SETTINGS_URL",
    "JAVA_FORMATTER_PROFILE_NAME",
    "JAVA_CODEGENERATION_INSERTIONLOCATION",
    "JAVA_CODEGENERATION_TOSTRING_TEMPLATE",
    "JAVA_PROJECT_OUTPUT_PATH_KEY",
}


# ---------------------------------------------------------------------------
# Step 4: Parse constructor defaults
# ---------------------------------------------------------------------------

def parse_constructor_defaults(source: str) -> dict:
    """Parse default values from the Preferences() constructor."""
    defaults = {}

    # Find constructor body
    ctor_match = re.search(r"public\s+Preferences\s*\(\s*\)\s*\{", source)
    if not ctor_match:
        return defaults

    # Extract constructor body (rough heuristic: find matching brace)
    start = ctor_match.end()
    depth = 1
    pos = start
    while pos < len(source) and depth > 0:
        if source[pos] == "{":
            depth += 1
        elif source[pos] == "}":
            depth -= 1
        pos += 1
    ctor_body = source[start : pos - 1]

    for m in CONSTRUCTOR_DEFAULT_BOOL_RE.finditer(ctor_body):
        defaults[m.group(1)] = m.group(2) == "true"
    for m in CONSTRUCTOR_DEFAULT_STR_RE.finditer(ctor_body):
        defaults[m.group(1)] = m.group(2)
    for m in CONSTRUCTOR_DEFAULT_INT_RE.finditer(ctor_body):
        defaults[m.group(1)] = int(m.group(2))

    return defaults


# Map from Java field names to constant names for default lookups
FIELD_TO_CONST = {
    "referencesCodeLensEnabled": "REFERENCES_CODE_LENS_ENABLED_KEY",
    "implementationsCodeLens": "IMPLEMENTATIONS_CODE_LENS_KEY",
    "javaFormatEnabled": "JAVA_FORMAT_ENABLED_KEY",
    "javaQuickFixShowAt": "QUICK_FIX_SHOW_AT",
    "javaFormatOnTypeEnabled": "JAVA_FORMAT_ON_TYPE_ENABLED_KEY",
    "javaSaveActionsOrganizeImportsEnabled": "JAVA_SAVE_ACTIONS_ORGANIZE_IMPORTS_KEY",
    "javaUpdateImportsOnPasteEnabled": "JAVA_UPDATE_IMPORTS_ON_PASTE_ENABLED_KEY",
    "signatureHelpEnabled": "SIGNATURE_HELP_ENABLED_KEY",
    "signatureHelpDescriptionEnabled": "SIGNATURE_HELP_DESCRIPTION_ENABLED_KEY",
    "hoverJavadocEnabled": "JAVA_HOVER_JAVADOC_ENABLED_KEY",
    "renameEnabled": "RENAME_ENABLED_KEY",
    "executeCommandEnabled": "EXECUTE_COMMAND_ENABLED_KEY",
    "autobuildEnabled": "AUTOBUILD_ENABLED_KEY",
    "completionEnabled": "COMPLETION_ENABLED_KEY",
    "postfixCompletionEnabled": "POSTFIX_COMPLETION_KEY",
    "completionOverwrite": "JAVA_COMPLETION_OVERWRITE_KEY",
    "foldingRangeEnabled": "FOLDINGRANGE_ENABLED_KEY",
    "selectionRangeEnabled": "SELECTIONRANGE_ENABLED_KEY",
    "collapseCompletionItems": "JAVA_COMPLETION_COLLAPSE_KEY",
    "javaFormatComments": "JAVA_FORMAT_COMMENTS",
    "hashCodeEqualsTemplateUseJava7Objects": "JAVA_CODEGENERATION_HASHCODEEQUALS_USEJAVA7OBJECTS",
    "hashCodeEqualsTemplateUseInstanceof": "JAVA_CODEGENERATION_HASHCODEEQUALS_USEINSTANCEOF",
    "codeGenerationTemplateUseBlocks": "JAVA_CODEGENERATION_USEBLOCKS",
    "codeGenerationTemplateGenerateComments": "JAVA_CODEGENERATION_GENERATECOMMENTS",
    "generateToStringSkipNullValues": "JAVA_CODEGENERATION_TOSTRING_SKIPNULLVALUES",
    "generateToStringListArrayContents": "JAVA_CODEGENERATION_TOSTRING_LISTARRAYCONTENTS",
    "generateToStringLimitElements": "JAVA_CODEGENERATION_TOSTRING_LIMITELEMENTS",
    "importGradleEnabled": "IMPORT_GRADLE_ENABLED",
    "importGradleOfflineEnabled": "IMPORT_GRADLE_OFFLINE_ENABLED",
    "gradleWrapperEnabled": "GRADLE_WRAPPER_ENABLED",
    "gradleAnnotationProcessingEnabled": "GRADLE_ANNOTATION_PROCESSING_ENABLED",
    "importMavenEnabled": "IMPORT_MAVEN_ENABLED",
    "mavenOffline": "IMPORT_MAVEN_OFFLINE",
    "mavenDisableTestClasspathFlag": "MAVEN_DISABLE_TEST_CLASSPATH_FLAG",
    "mavenDownloadSources": "MAVEN_DOWNLOAD_SOURCES",
    "eclipseDownloadSources": "ECLIPSE_DOWNLOAD_SOURCES",
    "mavenUpdateSnapshots": "MAVEN_UPDATE_SNAPSHOTS",
    "includeAccessors": "JAVA_REFERENCES_INCLUDE_ACCESSORS",
    "smartSemicolonDetection": "JAVA_EDIT_SMARTSEMICOLON_DETECTION",
    "includeDecompiledSources": "JAVA_REFERENCES_INCLUDE_DECOMPILED_SOURCES",
    "includeSourceMethodDeclarations": "JAVA_SYMBOLS_INCLUDE_SOURCE_METHOD_DECLARATIONS",
    "showGeneratedCodeSymbols": "JAVA_SYMBOLS_INCLUDE_GENERATED_CODE",
    "insertSpaces": "JAVA_CONFIGURATION_INSERTSPACES",
    "tabSize": "JAVA_CONFIGURATION_TABSIZE",
    "avoidVolatileChanges": "JAVA_CODEACTION_SORTMEMBER_AVOIDVOLATILECHANGES",
    "protobufSupportEnabled": "JAVA_JDT_LS_PROTOBUF_SUPPORT_ENABLED",
    "aspectjSupportEnabled": "JAVA_JDT_LS_ASPECTJ_SUPPORT_ENABLED",
    "kotlinSupportEnabled": "JAVA_JDT_LS_KOTLIN_SUPPORT_ENABLED",
    "groovySupportEnabled": "JAVA_JDT_LS_GROOVY_SUPPORT_ENABLED",
    "javacEnabled": "JAVA_JDT_LS_JAVAC_ENABLED",
    "androidSupportEnabled": "JAVA_JDT_LS_ANDROID_SUPPORT_ENABLED",
    "cleanUpActionsOnSaveEnabled": "JAVA_CLEANUPS_ACTIONS_ON_SAVE_CLEANUP",
    "extractInterfaceReplaceEnabled": "JAVA_REFACTORING_EXTRACT_INTERFACE_REPLACE",
    "telemetryEnabled": "JAVA_TELEMETRY_ENABLED_KEY",
    "validateAllOpenBuffersOnChanges": "JAVA_EDIT_VALIDATE_ALL_OPEN_BUFFERS_ON_CHANGES",
    "chainCompletionEnabled": "CHAIN_COMPLETION_KEY",
    "completionLazyResolveTextEditEnabled": "COMPLETION_LAZY_RESOLVE_TEXT_EDIT_ENABLED_KEY",
    "inlayHintsVariableTypesEnabled": "JAVA_INLAYHINTS_VARIABLETYPES_ENABLED",
    "inlayHintsParameterTypesEnabled": "JAVA_INLAYHINTS_PARAMETERTYPES_ENABLED",
    "inlayHintsFormatParametersEnabled": "JAVA_INLAYHINTS_FORMATPARAMETERS_ENABLED",
    "inlayHintsSuppressedWhenSameNameNumberedParameter": "JAVA_INLAYHINTS_PARAMETERNAMES_SUPPRESS_WHEN_SAME_NAME_NUMBERED",
}


# ---------------------------------------------------------------------------
# Step 5: Build the nested JSON Schema from flat dotted keys
# ---------------------------------------------------------------------------

# Properties that are not defined in Preferences.java but are known to be used
# in the wild (e.g. from the JDTLS wiki example or common configurations).
# These are injected as permissive pass-through entries so that
# additionalProperties: false can be set on ALL nodes (catching typos like
# "inlayhints" vs "inlayHints") without rejecting these legitimate keys.
#
# Format: { "dot.separated.path.relative.to.java": schema_dict }
KNOWN_EXTRA_PROPERTIES = {
    "errors": {
        "description": "Error reporting settings.",
        "type": "object",
        "properties": {
            "incompleteClasspath": {
                "description": "Incomplete classpath error settings.",
                "type": "object",
                "properties": {
                    "severity": {
                        "description": "Severity of the message when the classpath is incomplete for a Java file.",
                        "type": "string",
                        "enum": ["ignore", "info", "warning", "error"],
                        "default": "warning",
                    }
                },
                "additionalProperties": False,
            }
        },
        "additionalProperties": False,
    },
    "gradle": {
        "description": "Top-level Gradle settings (download sources).",
        "type": "object",
        "properties": {
            "downloadSources": {
                "description": "Whether to download Gradle dependency sources.",
                "type": "boolean",
            }
        },
        "additionalProperties": False,
    },
    "launch": {
        "description": "Launch mode for Java applications (e.g. 'hybrid').",
        "type": "string",
    },
}


def set_nested(d: dict, path: list, value: Any):
    """Set a value in a nested dict using a list of path segments."""
    for key in path[:-1]:
        if key not in d:
            d[key] = OrderedDict()
        d = d[key]
    d[path[-1]] = value


def make_nullable(prop: dict) -> dict:
    """Wrap a property schema to also accept null values."""
    if "type" in prop:
        t = prop["type"]
        if isinstance(t, list):
            if "null" not in t:
                prop["type"] = t + ["null"]
        else:
            prop["type"] = [t, "null"]
    elif "oneOf" in prop:
        has_null = any(
            entry.get("type") == "null" for entry in prop["oneOf"]
        )
        if not has_null:
            prop["oneOf"].append({"type": "null"})
    else:
        # Fallback: wrap in oneOf
        original = dict(prop)
        prop.clear()
        prop["oneOf"] = [original, {"type": "null"}]
    return prop


def build_property_schema(
    const_name: str,
    key: str,
    type_str: str,
    description: Optional[str],
    enum_values: Optional[list],
    default_value: Any = None,
) -> dict:
    """Build a JSON Schema property definition for a single preference key."""
    prop: dict = OrderedDict()

    if description:
        prop["description"] = description

    is_nullable = const_name in NULLABLE_CONSTANTS

    # Special cases for properties that accept multiple types
    if const_name == "JAVA_COMPLETION_GUESS_METHOD_ARGUMENTS_KEY":
        prop["oneOf"] = [
            {"type": "boolean"},
            {"type": "string", "enum": enum_values},
        ]
        if default_value is not None:
            prop["default"] = default_value
        if is_nullable:
            make_nullable(prop)
        return prop

    if const_name == "JAVA_PROJECT_REFERENCED_LIBRARIES_KEY":
        prop["oneOf"] = [
            {
                "type": "array",
                "items": {"type": "string"},
                "description": "Shortcut: an array of include patterns.",
            },
            {
                "type": "object",
                "properties": {
                    "include": {
                        "type": "array",
                        "items": {"type": "string"},
                    },
                    "exclude": {
                        "type": "array",
                        "items": {"type": "string"},
                    },
                    "sources": {
                        "type": "object",
                        "additionalProperties": {"type": "string"},
                        "description": "Map of library path to source path.",
                    },
                },
                "required": ["include"],
                "additionalProperties": False,
            },
        ]
        if default_value is not None:
            prop["default"] = default_value
        if is_nullable:
            make_nullable(prop)
        return prop

    if const_name == "JAVA_CONFIGURATION_RUNTIMES":
        prop["type"] = "array"
        prop["items"] = {
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The execution environment name (e.g. JavaSE-21).",
                    "enum": [
                        "J2SE-1.5",
                        "JavaSE-1.6",
                        "JavaSE-1.7",
                        "JavaSE-1.8",
                        "JavaSE-9",
                        "JavaSE-10",
                        "JavaSE-11",
                        "JavaSE-12",
                        "JavaSE-13",
                        "JavaSE-14",
                        "JavaSE-15",
                        "JavaSE-16",
                        "JavaSE-17",
                        "JavaSE-18",
                        "JavaSE-19",
                        "JavaSE-20",
                        "JavaSE-21",
                        "JavaSE-22",
                        "JavaSE-23",
                        "JavaSE-24",
                    ],
                },
                "path": {
                    "type": "string",
                    "description": "Path to the JDK installation.",
                },
                "javadoc": {
                    "type": "string",
                    "description": "Path or URL to Javadoc.",
                },
                "sources": {
                    "type": "string",
                    "description": "Path to source archive.",
                },
                "default": {
                    "type": "boolean",
                    "description": "Whether this is the default runtime.",
                },
            },
            "required": ["name", "path"],
            "additionalProperties": False,
        }
        return prop

    if const_name == "JAVA_CONFIGURATION_ASSOCIATIONS":
        prop["type"] = "object"
        prop["additionalProperties"] = {"type": "string"}
        prop["description"] = (
            prop.get("description", "")
            + " Map of glob patterns (e.g. '*.xyz') to language ids."
        ).strip()
        return prop

    if const_name == "JAVA_GRADLE_WRAPPER_SHA256_KEY":
        prop["type"] = "array"
        prop["items"] = {
            "type": "object",
            "properties": {
                "sha256": {"type": "string"},
                "allowed": {"type": "boolean"},
            },
        }
        return prop

    # Standard types
    if enum_values and type_str in ("string", None):
        prop["type"] = "string"
        prop["enum"] = enum_values
    elif type_str == "boolean":
        prop["type"] = "boolean"
    elif type_str == "integer":
        prop["type"] = "integer"
    elif type_str == "string[]":
        prop["type"] = "array"
        prop["items"] = {"type": "string"}
    elif type_str == "string":
        prop["type"] = "string"
    elif type_str == "object":
        prop["type"] = "object"
    else:
        # Fallback
        prop["type"] = "string"

    if default_value is not None:
        prop["default"] = default_value

    if is_nullable:
        make_nullable(prop)

    return prop


def nest_properties(flat_props: dict) -> dict:
    """
    Convert flat dotted keys like "java.import.gradle.enabled" into a nested
    JSON Schema structure with proper "properties" at each level.
    """
    # First, organize by path segments (stripping the leading "java.")
    tree = OrderedDict()
    for key, schema in flat_props.items():
        # All keys start with "java." - strip it since it will be under the "java" object
        if key.startswith("java."):
            path = key[5:].split(".")
        else:
            path = key.split(".")
        set_nested(tree, path, ("__leaf__", schema))

    def is_leaf_only(node: dict) -> bool:
        """Check if all children in a node are leaves (no further nesting)."""
        return all(
            isinstance(v, tuple) and v[0] == "__leaf__"
            for v in node.values()
        )

    def build_schema_node(node, path=""):
        """Recursively build JSON Schema from the tree structure."""
        if isinstance(node, tuple) and node[0] == "__leaf__":
            return node[1]

        result = OrderedDict()
        # Add description for intermediate grouping nodes if available
        if path and path in INTERMEDIATE_NODE_DESCRIPTIONS:
            result["description"] = INTERMEDIATE_NODE_DESCRIPTIONS[path]
        result["type"] = "object"
        props = OrderedDict()
        for key, value in sorted(node.items()):
            child_path = f"{path}.{key}" if path else key
            if isinstance(value, tuple) and value[0] == "__leaf__":
                props[key] = value[1]
            elif isinstance(value, dict):
                props[key] = build_schema_node(value, child_path)
            else:
                props[key] = value

        # Inject known extra properties that aren't in Preferences.java
        # but are legitimately used (wiki examples, common configs).
        # Only inject at the top level (path="" means we're at the java.* level).
        if path == "":
            for extra_key, extra_schema in KNOWN_EXTRA_PROPERTIES.items():
                if extra_key not in props:
                    props[extra_key] = extra_schema

        result["properties"] = props
        # Set additionalProperties: false on ALL nodes to catch typos
        # (e.g. "inlayhints" vs "inlayHints"). Known non-Preferences.java
        # keys are explicitly added via KNOWN_EXTRA_PROPERTIES above.
        result["additionalProperties"] = False
        return result

    return build_schema_node(tree)


# ---------------------------------------------------------------------------
# Step 6: Assemble the full schema
# ---------------------------------------------------------------------------

def generate_schema(source: str) -> dict:
    """Generate the complete JSON Schema from Preferences.java source."""
    constants = parse_constants(source)
    type_map = determine_types(source)
    enum_map = parse_enums(source)
    list_defaults = parse_list_defaults(source)
    int_defaults = parse_int_defaults(source)
    ctor_defaults = parse_constructor_defaults(source)

    # Map constant names -> default value names
    const_to_list_default = {
        "JAVA_IMPORT_EXCLUSIONS_KEY": "JAVA_IMPORT_EXCLUSIONS_DEFAULT",
        "JAVA_COMPLETION_FAVORITE_MEMBERS_KEY": "JAVA_COMPLETION_FAVORITE_MEMBERS_DEFAULT",
        "JAVA_IMPORT_ORDER_KEY": "JAVA_IMPORT_ORDER_DEFAULT",
        "JAVA_COMPLETION_FILTERED_TYPES_KEY": "JAVA_COMPLETION_FILTERED_TYPES_DEFAULT",
        "JAVA_RESOURCE_FILTERS": "JAVA_RESOURCE_FILTERS_DEFAULT",
    }

    const_to_int_default = {
        "JAVA_COMPLETION_MAX_RESULTS_KEY": "JAVA_COMPLETION_MAX_RESULTS_DEFAULT",
        "IMPORTS_ONDEMANDTHRESHOLD": "IMPORTS_ONDEMANDTHRESHOLD_DEFAULT",
        "IMPORTS_STATIC_ONDEMANDTHRESHOLD": "IMPORTS_STATIC_ONDEMANDTHRESHOLD_DEFAULT",
    }

    # Build flat property map: {dotted_key: json_schema_property}
    flat_props = OrderedDict()

    for const_name, info in constants.items():
        key = info["key"]
        description = info["description"]
        type_str = type_map.get(const_name)
        enum_values = enum_map.get(const_name)

        # Resolve default value
        default_value = None
        if const_name in const_to_list_default:
            default_name = const_to_list_default[const_name]
            if default_name in list_defaults:
                default_value = list_defaults[default_name]
        elif const_name in const_to_int_default:
            default_name = const_to_int_default[const_name]
            if default_name in int_defaults:
                default_value = int_defaults[default_name]

        # Try constructor defaults via field mapping
        if default_value is None:
            for field_name, mapped_const in FIELD_TO_CONST.items():
                if mapped_const == const_name and field_name in ctor_defaults:
                    default_value = ctor_defaults[field_name]
                    break

        prop_schema = build_property_schema(
            const_name, key, type_str, description, enum_values, default_value
        )
        flat_props[key] = prop_schema

    # Nest the flat java.* properties into a tree
    java_schema = nest_properties(flat_props)

    # Build the full InitializationOptions schema
    schema = OrderedDict()
    schema["$schema"] = "http://json-schema.org/draft-07/schema#"
    schema["$id"] = (
        "https://github.com/eclipse-jdtls/eclipse.jdt.ls/"
        "jdtls-initialization-options.schema.json"
    )
    schema["title"] = "JDTLS Initialization Options"
    schema["description"] = (
        "Schema for Eclipse JDT Language Server initialization options. "
        "Auto-generated from Preferences.java at "
        "https://github.com/eclipse-jdtls/eclipse.jdt.ls/blob/main/"
        "org.eclipse.jdt.ls.core/src/org/eclipse/jdt/ls/core/internal/preferences/Preferences.java"
    )
    schema["type"] = "object"
    schema["additionalProperties"] = False
    schema["properties"] = OrderedDict()
    schema["properties"]["bundles"] = {
        "type": "array",
        "description": "A list of Java LS extension bundles (paths to JARs).",
        "items": {"type": "string"},
    }
    schema["properties"]["workspaceFolders"] = {
        "type": "array",
        "description": "A list of workspace folders (as URIs).",
        "items": {"type": "string", "format": "uri"},
    }
    schema["properties"]["settings"] = OrderedDict()
    schema["properties"]["settings"]["type"] = "object"
    schema["properties"]["settings"]["description"] = (
        "Java LS configuration settings."
    )
    schema["properties"]["settings"]["additionalProperties"] = False
    schema["properties"]["settings"]["properties"] = OrderedDict()
    schema["properties"]["settings"]["properties"]["java"] = java_schema

    return schema


# ---------------------------------------------------------------------------
# Step 7: Post-process / validate
# ---------------------------------------------------------------------------

def validate_schema(schema: dict):
    """Optionally validate the generated schema if jsonschema is available."""
    try:
        import jsonschema
        jsonschema.Draft7Validator.check_schema(schema)
        print("Schema validation: PASSED (valid JSON Schema Draft 7)", file=sys.stderr)
        return True
    except ImportError:
        print(
            "Note: install 'jsonschema' package to enable schema self-validation.",
            file=sys.stderr,
        )
        return True
    except Exception as e:
        print(f"Schema validation FAILED: {e}", file=sys.stderr)
        return False


def print_stats(schema: dict, constants: dict, type_map: dict):
    """Print generation statistics."""
    java_props = schema["properties"]["settings"]["properties"]["java"]["properties"]

    def count_leaves(node):
        if "properties" in node:
            total = 0
            for v in node["properties"].values():
                total += count_leaves(v)
            return total
        return 1

    leaf_count = count_leaves(schema["properties"]["settings"]["properties"]["java"])

    print(f"Extracted {len(constants)} preference constants from Preferences.java", file=sys.stderr)
    print(f"Determined types for {len(type_map)} preferences", file=sys.stderr)
    print(f"Generated schema with {leaf_count} leaf properties", file=sys.stderr)
    print(f"Top-level java.* categories: {list(java_props.keys())}", file=sys.stderr)


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(
        description="Generate JSON Schema for JDTLS initialization options from Preferences.java"
    )
    parser.add_argument(
        "--input",
        "-i",
        help="Path to a local Preferences.java file. If not provided, fetches from GitHub.",
    )
    parser.add_argument(
        "--output",
        "-o",
        default="../jdtls-initialization-options.schema.json",
        help="Output path for the generated schema (default: ../jdtls-initialization-options.schema.json)",
    )
    parser.add_argument(
        "--stdout",
        action="store_true",
        help="Print the schema to stdout instead of writing to a file.",
    )
    parser.add_argument(
        "--validate",
        action="store_true",
        default=True,
        help="Validate the generated schema (requires jsonschema package).",
    )
    args = parser.parse_args()

    # Load source
    if args.input:
        print(f"Reading {args.input} ...", file=sys.stderr)
        with open(args.input, "r") as f:
            source = f.read()
    else:
        source = fetch_preferences_java(PREFERENCES_URL)

    # Parse and generate
    constants = parse_constants(source)
    type_map = determine_types(source)
    schema = generate_schema(source)

    # Stats
    print_stats(schema, constants, type_map)

    # Validate
    if args.validate:
        validate_schema(schema)

    # Output
    output_json = json.dumps(schema, indent=2, ensure_ascii=False)

    if args.stdout:
        print(output_json)
    else:
        with open(args.output, "w") as f:
            f.write(output_json)
            f.write("\n")
        print(f"Schema written to {args.output}", file=sys.stderr)


if __name__ == "__main__":
    main()
