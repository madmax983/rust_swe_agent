use maxwells_daemon::cli::args::CatalogCmd;
use maxwells_daemon::cli::catalog::run_catalog;

#[test]
fn test_catalog_render() {
    let cmd = CatalogCmd {
        free_only: false,
        stage: None,
        format: "text".to_string(),
    };
    let _ = run_catalog(cmd);
}

#[test]
fn test_catalog_render_json() {
    let cmd = CatalogCmd {
        free_only: false,
        stage: None,
        format: "json".to_string(),
    };
    let _ = run_catalog(cmd);
}

#[test]
fn test_catalog_render_filtered() {
    let cmd = CatalogCmd {
        free_only: true,
        stage: Some("preflight".to_string()),
        format: "text".to_string(),
    };
    let _ = run_catalog(cmd);
}
