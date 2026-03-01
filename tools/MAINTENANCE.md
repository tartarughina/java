# Maintenance Guide for `generate-schema.py`

This document describes how to keep `generate-schema.py` aligned with upstream changes to
[`Preferences.java`](https://github.com/eclipse-jdtls/eclipse.jdt.ls/blob/main/org.eclipse.jdt.ls.core/src/org/eclipse/jdt/ls/core/internal/preferences/Preferences.java)
in the Eclipse JDT Language Server.

---

## How the script works

The script extracts preference definitions from `Preferences.java` and produces a JSON Schema
(`jdtls-initialization-options.schema.json`) that validates the `initializationOptions` payload
sent during the LSP `initialize` request.

### What is auto-extracted (no manual work needed)

| Data | How it's extracted |
|---|---|
| **Preference keys** | Regex on `public static final String ... = "java.*";` constants |
| **Types** (boolean, string, int, list) | By matching which `getBoolean()`, `getString()`, `getInt()`, `getList()`, or `getValue()` call reads each constant inside `updateFrom()` |
| **Javadoc descriptions** | Parsed from `/** */` or `//` comments preceding each constant declaration |
| **List defaults** | Parsed from `*_DEFAULT.add("...")` calls in the static initializer |
| **Int defaults** | Parsed from `public static final int *_DEFAULT = N;` declarations |
| **Constructor defaults** | Parsed from simple assignments (`field = value;`) in the `Preferences()` constructor |

### What is hardcoded (requires manual updates)

There are **eight** hardcoded sections in the script that cannot be reliably auto-extracted
because the values live in **separate Java files** or involve **complex parsing logic** that
goes beyond what the script's regex approach can handle.

#### 1. `parse_enums()` — Enum / mode allowed values

**Location:** ~line 206  
**What:** Maps constant names to their allowed string values.

These values come from Java enums and mode classes defined in **other files**, not in
`Preferences.java` itself. Examples:

| Constant | Source class |
|---|---|
| `CONFIGURATION_UPDATE_BUILD_CONFIGURATION_KEY` | `FeatureStatus` (in Preferences.java) |
| `JAVA_COMPILE_NULLANALYSIS_MODE` | `FeatureStatus` (in Preferences.java) |
| `IMPLEMENTATIONS_CODE_LENS_KEY` | String values parsed in setter logic |
| `JAVA_INLAYHINTS_PARAMETERNAMES_ENABLED` | `InlayHintsParameterMode` (separate file) |
| `JAVA_PROJECT_ENCODING` | `ProjectEncodingMode` (separate file) |
| `COMPLETION_MATCH_CASE_MODE_KEY` | `CompletionMatchCaseMode` (separate file) |
| `JAVA_COMPLETION_GUESS_METHOD_ARGUMENTS_KEY` | `CompletionGuessMethodArgumentsMode` (separate file) |
| `JAVA_SEARCH_SCOPE` | `SearchScope` (in Preferences.java) |
| `JAVA_CODEGENERATION_TOSTRING_CODESTYLE` | String constants in toString handler |
| `JAVA_CODEGENERATION_INSERTIONLOCATION` | String constants in code generation handler |
| `JAVA_CODEGENERATION_ADD_FINAL_FOR_NEW_DECLARATION` | String constants in code generation handler |
| `MAVEN_NOT_COVERED_PLUGIN_EXECUTION_SEVERITY` | Severity-like string values |
| `MAVEN_DEFAULT_MOJO_EXECUTION_ACTION` | Validated in `setMavenDefaultMojoExecutionAction()` |

**When to update:** A new preference uses an enum/mode type, or an existing enum gains/loses values.

**How to update:** Add or modify the entry in the `enums` dict inside `parse_enums()`:
```python
enums["NEW_CONSTANT_NAME"] = ["value1", "value2", "value3"]
```

**How to find the values:** Look at the enum/mode class referenced in the `updateFrom()` block
for that preference. For example, if you see `SomeMode.fromString(...)`, find the `SomeMode`
enum and list its values.

#### 2. `NULLABLE_CONSTANTS` — Properties that accept `null`

**Location:** ~line 263  
**What:** A set of constant names whose values may legitimately be `null` in JSON.

JSON Schema's `"type": "string"` rejects `null` by default. This set tells the generator to
emit `"type": ["string", "null"]` instead.

**When to update:** A new string/array preference is added where users commonly set the value
to `null` (e.g., `"preferred": null`, `"userSettings": null`).

**How to update:** Add the constant name to the set:
```python
NULLABLE_CONSTANTS = {
    ...,
    "NEW_NULLABLE_CONSTANT_KEY",
}
```

**How to identify candidates:** Look for properties where the `updateFrom()` code passes a
`null` default or where the wiki/documentation examples use `null`.

#### 3. `FIELD_TO_CONST` — Constructor field-to-constant mapping

**Location:** ~line 313  
**What:** Maps Java field names (as written in the `Preferences()` constructor) to their
corresponding `public static final String` constant names.

This is needed because the constructor sets defaults like `autobuildEnabled = true;` using
the **field name**, not the constant name. The script parses these assignments but needs
this map to associate `autobuildEnabled` → `AUTOBUILD_ENABLED_KEY` → `"java.autobuild.enabled"`.

**When to update:** A new preference is added that has a default value set in the constructor.

**How to update:** Add a mapping from the field name to the constant name:
```python
FIELD_TO_CONST = {
    ...,
    "newFieldName": "NEW_CONSTANT_NAME",
}
```

**How to find the field name:** Look at the `Preferences()` constructor for the line that
initializes the field (e.g., `newFieldName = someDefault;`), then find which constant it
corresponds to by searching which `containsKey(configuration, SOME_KEY)` block in `updateFrom()`
sets that same field.

#### 4. `build_property_schema()` — Special-case type overrides

**Location:** ~line 415  
**What:** Hardcoded schema definitions for properties that don't fit the standard
boolean/string/int/list pattern.

Currently there are five special cases:

| Constant | Why it's special |
|---|---|
| `JAVA_COMPLETION_GUESS_METHOD_ARGUMENTS_KEY` | Accepts **both** `boolean` and enum `string` (`oneOf`) |
| `JAVA_PROJECT_REFERENCED_LIBRARIES_KEY` | Accepts **either** a `string[]` shortcut or a structured `{include, exclude, sources}` object |
| `JAVA_CONFIGURATION_RUNTIMES` | Array of structured objects with `name`, `path`, `javadoc`, `sources`, `default` |
| `JAVA_CONFIGURATION_ASSOCIATIONS` | Object map (`{"*.ext": "java"}`) rather than a simple type |
| `JAVA_GRADLE_WRAPPER_SHA256_KEY` | Array of objects with `sha256` and `allowed` fields |

**When to update:** A new preference has a complex/union type, or an existing special case
changes its structure (e.g., runtimes gain a new field).

**How to update:** Add a new `if const_name == "..."` block in `build_property_schema()`
before the standard type handling, returning the custom schema dict.

#### 5. `generate_schema()` — Default value mappings

**Location:** ~line 643 (`const_to_list_default`) and ~line 652 (`const_to_int_default`)  
**What:** Maps preference constant names to their corresponding `*_DEFAULT` constant names
for list and int defaults.

**When to update:** A new preference is added that has a named default constant
(e.g., `public static final List<String> NEW_PREF_DEFAULT;` populated in the static initializer).

**How to update:** Add the mapping:
```python
const_to_list_default = {
    ...,
    "NEW_PREF_KEY": "NEW_PREF_DEFAULT",
}
```

#### 6. `JAVA_CONFIGURATION_RUNTIMES` — ExecutionEnvironment enum values

**Location:** Inside `build_property_schema()`, ~line 490  
**What:** The `"enum"` list for `configuration.runtimes[].name` (e.g., `"JavaSE-21"`, `"JavaSE-22"`).

**When to update:** A new Java version is released and JDTLS adds it to the
`ExecutionEnvironment` enum (in `RuntimeEnvironment.java` or similar).

**How to update:** Append the new value to the `"enum"` list:
```python
"enum": [
    ...,
    "JavaSE-25",
],
```

#### 7. `DESCRIPTION_OVERRIDES` — Fallback descriptions for undocumented constants

**Location:** ~line 120  
**What:** A dict mapping constant names to human-written descriptions for constants that
have no Javadoc or line comment in `Preferences.java`.

Currently 29 constants lack upstream documentation. The script first tries to extract a
description from the source; if none is found, it falls back to this dict.

**When to update:** A new preference is added to `Preferences.java` without any Javadoc
or comment above it.

**How to detect:** After regenerating, run:
```sh
python3 -c "
import json
schema = json.load(open('jdtls-initialization-options.schema.json'))
def find_missing(node, path=''):
    missing = []
    if 'properties' in node:
        for key, value in node['properties'].items():
            current = f'{path}.{key}' if path else key
            if 'properties' in value:
                missing.extend(find_missing(value, current))
            elif 'description' not in value:
                missing.append(f'java.{current}')
    return missing
java_node = schema['properties']['settings']['properties']['java']
for m in sorted(find_missing(java_node)):
    print(f'  {m}')
"
```

If any properties are printed, they need a fallback description.

**How to update:** Add the constant name and description to the dict:
```python
DESCRIPTION_OVERRIDES = {
    ...,
    "NEW_UNDOCUMENTED_CONSTANT": (
        "Description of what this preference controls."
    ),
}
```

**Note:** If a future version of `Preferences.java` adds Javadoc to a constant that already
has an entry in `DESCRIPTION_OVERRIDES`, the upstream Javadoc will take precedence automatically.
The override only applies when `extract_javadoc_before()` returns `None`.

#### 8. `INTERMEDIATE_NODE_DESCRIPTIONS` — Descriptions for grouping objects

**Location:** ~line 120  
**What:** A dict mapping dotted path segments (relative to `java.`) to descriptions for
intermediate grouping objects — nodes in the schema that are structural containers rather
than actual preference keys.

For example, `"signatureHelp"` groups `signatureHelp.enabled` and
`signatureHelp.description.enabled`. Without an entry here, hovering over `"signatureHelp"`
in the JSON file shows no tooltip.

The dict handles both top-level groups (e.g. `"completion"`, `"format"`) and nested
intermediates (e.g. `"compile.nullAnalysis"`, `"import.gradle.wrapper"`).

**When to update:** A new preference is added whose dotted key introduces a new intermediate
path segment that doesn't already exist in the schema. For example, if JDTLS added
`java.newFeature.someSetting`, you'd need an entry for `"newFeature"`.

**How to detect:** After regenerating, run:
```sh
python3 -c "
import json
schema = json.load(open('jdtls-initialization-options.schema.json'))
def find_missing(node, path=''):
    missing = []
    if 'properties' in node:
        for key, value in node['properties'].items():
            current = f'{path}.{key}' if path else key
            if 'properties' in value and 'description' not in value:
                missing.append(f'java.{current}')
            if 'properties' in value:
                missing.extend(find_missing(value, current))
    return missing
java_node = schema['properties']['settings']['properties']['java']
for m in sorted(find_missing(java_node)):
    print(f'  {m}')
"
```

If any paths are printed, they need a description.

**How to update:** Add the path (relative to `java.`) and a description to the dict:
```python
INTERMEDIATE_NODE_DESCRIPTIONS = {
    ...,
    "newFeature": "New feature settings.",
    "newFeature.subGroup": "Sub-group settings within new feature.",
}
```

---

## Step-by-step: Updating for a new `Preferences.java` release

### 1. Regenerate and diff

```sh
cd java
python3 tools/generate-schema.py --output jdtls-initialization-options.schema.json
```

If the upstream `Preferences.java` has only added new `public static final String` constants
with standard types (boolean/string/int/list), the regeneration handles everything automatically.

### 2. Check for new constants that need manual attention

Compare the previous and new `Preferences.java` to identify changes. Focus on the
`updateFrom()` method — every preference is read there.

For each **new** preference, ask:

| Question | If yes → action |
|---|---|
| Does it use `getValue()` or have complex parsing? | Add a special case in `build_property_schema()` |
| Does it call `SomeEnum.fromString()`? | Add allowed values in `parse_enums()` |
| Can the value be `null`? | Add to `NULLABLE_CONSTANTS` |
| Does it have a named `*_DEFAULT` list/int constant? | Add to `const_to_list_default` or `const_to_int_default` |
| Does the constructor set a default for it? | Add to `FIELD_TO_CONST` |
| Is there no Javadoc or comment above the declaration? | Add to `DESCRIPTION_OVERRIDES` |

### 3. Check for modified enums

If an existing enum class gained or lost values (e.g., a new `CompletionMatchCaseMode` option),
update the corresponding entry in `parse_enums()`.

### 4. Check for missing descriptions

After regenerating, check if any properties ended up without a description:

```sh
python3 -c "
import json
schema = json.load(open('jdtls-initialization-options.schema.json'))
def find_missing(node, path=''):
    missing = []
    if 'properties' in node:
        for key, value in node['properties'].items():
            current = f'{path}.{key}' if path else key
            if 'properties' in value:
                missing.extend(find_missing(value, current))
            elif 'description' not in value:
                missing.append(f'java.{current}')
    return missing
java_node = schema['properties']['settings']['properties']['java']
for m in sorted(find_missing(java_node)):
    print(f'  {m}')
"
```

If any are printed, add entries for them in `DESCRIPTION_OVERRIDES` in the script,
then regenerate again.

### 5. Check for new Java versions

If a new `JavaSE-XX` was added to the `ExecutionEnvironment` enum, append it to the runtimes
enum list in `build_property_schema()`.

### 6. Validate

After regenerating, run the validation:

```sh
python3 -c "
import json, jsonschema
schema = json.load(open('jdtls-initialization-options.schema.json'))
jsonschema.Draft7Validator.check_schema(schema)
print('Schema is valid JSON Schema Draft 7')
"
```

Also validate your own `jdtls-initialization-options.json` against the new schema:

```sh
python3 -c "
import json, jsonschema
schema = json.load(open('jdtls-initialization-options.schema.json'))
options = json.load(open('jdtls-initialization-options.json'))
jsonschema.validate(instance=options, schema=schema)
print('Options file is valid')
"
```

### 7. Validate against the wiki example

The [JDTLS wiki](https://github.com/eclipse-jdtls/eclipse.jdt.ls/wiki/Running-the-JAVA-LS-server-from-the-command-line#initialize-request)
includes an example `initializationOptions` payload. It's worth testing against it as a
smoke test. Note that the wiki example may reference properties not in `Preferences.java`
(like `errors.incompleteClasspath.severity`) — those will pass validation because intermediate
grouping objects allow additional properties by design.

---

## Known limitations

1. **Enum values are not auto-extracted.** They live in separate Java files
   (`InlayHintsParameterMode.java`, `CompletionMatchCaseMode.java`, etc.) that the script
   does not fetch. The `FeatureStatus` and `SearchScope` enums inside `Preferences.java`
   could theoretically be auto-parsed, but for consistency all enums are maintained in one place.

2. **The `errors.incompleteClasspath.severity` key** appears in the wiki example but has no
   corresponding constant in `Preferences.java`. The schema handles this gracefully by not
   setting `additionalProperties: false` on intermediate grouping objects.

3. **The `JAVA_CLEANUPS_ACTIONS` constant** maps to `"java.cleanup.actions"` and accepts a list
   of strings representing cleanup action IDs. The valid action IDs are not enumerated in
   `Preferences.java`; they are defined elsewhere in the JDT codebase. The schema types this
   as `string[]` without constraining the values.

4. **Field-to-constant mapping is manual.** The Java field names in the constructor
   (e.g., `autobuildEnabled`) don't follow a predictable naming convention relative to their
   constant names (e.g., `AUTOBUILD_ENABLED_KEY`), so this mapping must be maintained by hand.

---

## Dependencies

The script requires **Python 3.8+** with no third-party dependencies for generation.

For **validation** (optional but recommended), install:

```sh
pip install jsonschema
```
