fetch-schema:
    curl -s "https://docs.x.ai/openapi.json" > openapi.json

generate-xai:
    jq -f xai-schemas.jq openapi.json > xai-schemas.json
    cargo run -p gen-xai && rustfmt --edition 2024 src/xai.rs
