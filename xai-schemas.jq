# Turns openapi.json into a JSON Schema document for typify, pruned to the
# transitive closure of the schemas src/grok.rs actually uses.
#
#   jq -f xai-schemas.jq openapi.json > xai-schemas.json
#
# Add a root here when grok.rs starts calling a new endpoint.

def roots:
  [
    "ModelRequest",            # POST /v1/responses      request
    "ModelResponse",           # POST /v1/responses      response
    "GenerateImageRequest",    # POST /v1/images/generations request
    "GeneratedImageResponse",  # POST /v1/images/generations response
    "ListModelsResponse"       # GET  /v1/models              response
  ];

# Every schema name referenced anywhere inside a given schema.
def refs_of($defs; $name):
  [ $defs[$name] | .. | objects | select(has("$ref")) | .["$ref"]
    | sub("^#/components/schemas/"; "") ];

# Flatten `allOf` into a single object schema, dereferencing any `$ref` members.
#
# ModelTool's variants are `allOf: [{$ref: FunctionDefinition}, {type: const}]`,
# which hides the const `type` discriminator from typify and makes the whole
# oneOf `#[serde(untagged)]`. Merged, every variant carries a required const
# `type` and typify can emit a tagged enum.
def merge_allof($defs):
  walk(
    if type == "object" and (.allOf | type) == "array"
    then
      if (.allOf | length) == 1
      then
        # A lone wrapper: unwrap it, keeping any `$ref` intact so the schema
        # still points at the named type rather than inlining a copy of it.
        del(.allOf) + .allOf[0]
      else
        ([.allOf[]
          | if has("$ref")
            then ($defs[(.["$ref"] | sub("^#/\\$defs/"; ""))] // {})
            else . end]) as $parts
        | reduce $parts[] as $p (del(.allOf);
            . + ($p | del(.properties, .required, .description))
            + { properties: ((.properties // {}) + ($p.properties // {})),
                required: (((.required // []) + ($p.required // [])) | unique) })
      end
    else . end
  );

# Fixpoint: grow the seed set until no new references appear.
def closure($defs):
  def step($acc):
    (($acc + ([ $acc[] | refs_of($defs; .) ] | flatten)) | unique) as $next
    | if ($next | length) == ($acc | length) then $acc else step($next) end;
  step(roots | unique);

.components.schemas as $defs
| closure($defs) as $keep
| {
    "$schema": "https://json-schema.org/draft/2020-12/schema",
    "title": "XaiSpec",
    "type": "object",
    "$defs": ($defs | with_entries(select(.key as $k | $keep | index($k))))
  }
| walk(
    if type == "object" and has("$ref")
    then .["$ref"] |= sub("#/components/schemas/"; "#/$defs/")
    else . end
  )
| merge_allof(.["$defs"])
# Drop `default`. typify turns a default into `#[serde(default = "...")]` with
# no `skip_serializing_if`, so the field is sent on every request -- and xAI
# rejects some of them outright ("Argument not supported: background") or lets
# them silently override its own defaults (temperature, top_p). No schema here
# has a property named "default", so this only strips the keyword.
| walk(if type == "object" then del(.default) else . end)
