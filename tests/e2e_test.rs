// E2E tests for the GeoJSON vector export pipeline.
//
// These tests run the full pullauta pipeline on real LiDAR data and validate
// the GeoJSON output structure against the expected FeatureCollection shape.
//
// The real-data test downloads a LAZ tile from the French government LiDAR HD
// service (data.geopf.fr). It is ignored by default to avoid slow CI runs on
// every push. Run with:
//   cargo test --test e2e_test -- --ignored --nocapture
//
// The CI workflow (.github/workflows/e2e.yml) runs these tests on push/PR.

use std::path::Path;
use std::process::Command;

/// Run the pullauta binary on the given input file.
fn run_pullauta(input: &Path) -> std::io::Result<std::process::Output> {
    let bin = env!("CARGO_BIN_EXE_pullauta");
    Command::new(bin)
        .arg(input)
        .env("RUST_LOG", "warn")
        .output()
}

/// Parse a GeoJSON file and assert it is a FeatureCollection with features.
fn assert_valid_feature_collection(path: &Path) {
    let content = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let val: serde_json::Value = serde_json::from_str(&content)
        .unwrap_or_else(|e| panic!("failed to parse JSON from {}: {e}", path.display()));
    assert_eq!(
        val["type"],
        "FeatureCollection",
        "{}: type is not FeatureCollection",
        path.display()
    );
    let features = val["features"]
        .as_array()
        .unwrap_or_else(|| panic!("{}: features is not an array", path.display()));
    assert!(
        !features.is_empty(),
        "{}: features array is empty",
        path.display()
    );
    for (i, f) in features.iter().enumerate() {
        assert_eq!(
            f["type"],
            "Feature",
            "{}: feature[{i}] type is not Feature",
            path.display()
        );
        assert!(
            f["geometry"]["type"].is_string(),
            "{}: feature[{i}] has no geometry type",
            path.display()
        );
    }
}

/// Set up the test: copy config to pullauta.ini, return cleanup guard.
struct TestEnv;
impl TestEnv {
    fn setup() -> Self {
        std::fs::copy("tests/e2e_config.ini", "pullauta.ini").expect("failed to copy e2e config");
        TestEnv
    }
}
impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = std::fs::remove_file("pullauta.ini");
        let _ = std::fs::remove_dir_all("temp");
        let _ = std::fs::remove_dir_all("e2e_out");
    }
}

/// Real-data e2e test: downloads a LAZ tile from the French LiDAR HD service
/// and runs the full pipeline with vector export enabled. Validates that
/// GeoJSON output files are valid FeatureCollections with ISOM property codes.
///
/// Ignored by default; run with:
///   cargo test --test e2e_test -- --ignored --nocapture
#[test]
#[ignore]
fn real_laz_produces_valid_geojson() {
    let _env = TestEnv::setup();

    let url = "https://data.geopf.fr/telechargement/download/LiDARHD-NUALID/NUALHD_1-0__LAZ_LAMB93_EP_2025-09-24/LHD_FXX_0338_6285_PTS_LAMB93_IGN69.copc.laz";
    let laz_path = Path::new("test_file.laz");

    if !laz_path.exists() {
        let status = Command::new("curl")
            .args(["-L", "-o", laz_path.to_str().unwrap(), url])
            .status()
            .expect("failed to download LAZ file");
        assert!(status.success(), "failed to download test LAZ file");
    }

    let output = run_pullauta(laz_path).expect("failed to execute pullauta");
    assert!(
        output.status.success(),
        "pullauta failed on real LAZ:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // validate contours.geojson in temp
    let contours = Path::new("temp/contours.geojson");
    if contours.exists() {
        assert_valid_feature_collection(contours);
        let content = std::fs::read_to_string(contours).unwrap();
        let val: serde_json::Value = serde_json::from_str(&content).unwrap();
        let features = val["features"].as_array().unwrap();
        let has_isom = features.iter().any(|f| f["properties"]["isom"].is_string());
        assert!(has_isom, "contours.geojson: no feature has isom property");
    }

    // validate output.geojson from batch merge if it exists
    let output_geojson = Path::new("e2e_out/output.geojson");
    if output_geojson.exists() {
        assert_valid_feature_collection(output_geojson);
    }
}
