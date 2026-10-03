use std::{env, fs, path::Path};

fn main() {
    println!("cargo::rerun-if-changed=schema/geojson.schema.json");

    let content = fs::read_to_string("schema/geojson.schema.json")
        .expect("failed to read schema/geojson.schema.json");
    let schema: schemars::schema::RootSchema =
        serde_json::from_str(&content).expect("failed to parse schema");

    let mut type_space = typify::TypeSpace::new(&typify::TypeSpaceSettings::default());
    type_space
        .add_root_schema(schema)
        .expect("failed to generate types");

    let tokens = type_space.to_stream();
    let file = syn::parse2::<syn::File>(tokens).expect("failed to parse generated tokens");
    let contents = prettyplease::unparse(&file);

    let out_dir = env::var("OUT_DIR").unwrap();
    let out_path = Path::new(&out_dir).join("geojson_types.rs");
    fs::write(&out_path, contents).expect("failed to write geojson_types.rs");
}
