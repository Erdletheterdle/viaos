//! Against the real `docker compose`, which needs no daemon to read a file.

use std::fs;
use std::path::Path;

use via_compose::Project;
use via_core::{Mount, Platform, Readiness};

fn write(dir: &Path, path: &str, text: &str) {
    let path = dir.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn open(dir: &Path) -> Project {
    Project::open(
        dir.join("compose.yaml"),
        "app",
        Platform::new("linux", "amd64"),
        Vec::new(),
        "7",
    )
    .unwrap()
}

const FILE: &str = r#"
name: not-this-one
x-via:
  variables:
    API_KEY: {secret: true, description: "for the API"}
    GREETING: {default: hi}
services:
  web:
    extends: {file: base.yaml, service: base}
    build: .
    env_file: web.env
    environment:
      - API_KEY
      - GREETING
      - URL=http://${HOST:-localhost}/${TOKEN:?needed}
      - LITERAL=$$KEEP
    ports: ["8080:80"]
    volumes:
      - ./conf:/etc/conf:ro
      - data:/data
      - /dev/ttyUSB0:/dev/ttyUSB0
    depends_on:
      migrate: {condition: service_completed_successfully}
    deploy: {replicas: 2}
    configs: [settings]
  migrate:
    image: alpine:3
    command: "true"
  hidden:
    image: alpine:3
    profiles: [debug]
configs:
  settings: {file: ./settings.ini}
volumes:
  data:
"#;

fn project(dir: &Path) -> Project {
    write(dir, "compose.yaml", FILE);
    write(
        dir,
        "base.yaml",
        "services:\n  base:\n    restart: always\n    healthcheck:\n      test: [\"CMD\", \"true\"]\n",
    );
    write(dir, "web.env", "FROM_FILE=1\n");
    write(dir, "conf/app.conf", "a\n");
    write(dir, "conf/sub/more.conf", "b\n");
    write(dir, "conf/.git/config", "no\n");
    write(dir, "settings.ini", "x\n");
    write(dir, ".env", "HOST=dotenv\n");
    write(dir, "unrelated.txt", "stays home\n");
    open(dir)
}

#[test]
fn the_release_carries_what_the_project_reads_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let (files, warnings) = project(dir.path()).files().unwrap();
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        vec![
            ".env",
            "conf/app.conf",
            "conf/sub/more.conf",
            "settings.ini",
            "web.env"
        ]
    );
    assert!(warnings.iter().any(|w| w.message.contains(".env")));
}

#[test]
fn a_path_outside_the_project_is_refused_at_build() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "compose.yaml",
        "services:\n  a:\n    image: alpine:3\n    volumes: [\"../elsewhere:/x\"]\n",
    );
    let err = open(dir.path()).files().unwrap_err().to_string();
    assert!(err.contains("outside the project"), "{err}");
}

#[test]
fn what_the_device_runs_has_nothing_to_build_and_is_still_unresolved() {
    let dir = tempfile::tempdir().unwrap();
    let text = project(dir.path()).device_compose().unwrap();
    assert!(!text.contains("build:"), "{text}");
    assert!(text.contains("image: app/web:7"), "{text}");
    assert!(text.contains("${TOKEN:?needed}"), "{text}");
    assert!(text.contains("./conf"), "{text}");
    // `extends` is flattened, so base.yaml needn't travel.
    assert!(text.contains("restart: always"), "{text}");
    assert!(!text.contains("hidden"), "{text}");
}

#[test]
fn metadata_comes_from_composes_own_reading() {
    let dir = tempfile::tempdir().unwrap();
    let (d, _) = project(dir.path()).deployment().unwrap();
    let web = &d.services["web"];

    assert_eq!(web.image.reference, "app/web:7");
    assert_eq!(web.replicas, 2);
    assert_eq!(web.readiness, Readiness::Healthy);
    assert_eq!(d.services["migrate"].readiness, Readiness::Completed);
    assert_eq!(web.ports[0].host_port, Some(8080));
    assert_eq!(web.environment["FROM_FILE"], "1");
    assert_eq!(web.environment["LITERAL"], "$KEEP");
    assert!(
        !web.environment.contains_key("URL"),
        "the device resolves it"
    );
    assert_eq!(web.runtime_env, vec!["API_KEY", "GREETING"]);
    assert!(
        web.mounts
            .iter()
            .any(|m| matches!(m, Mount::Volume { source, .. } if source == "data"))
    );
    assert_eq!(d.volumes["data"].name.as_deref(), Some("app_data"));

    let var = |k: &str| d.variables.iter().find(|v| v.key == k).unwrap();
    assert!(var("API_KEY").secret);
    assert_eq!(var("GREETING").default.as_deref(), Some("hi"));
    assert!(var("TOKEN").required, "`:?` has no fallback");
    assert_eq!(var("HOST").default.as_deref(), Some("localhost"));
    assert!(!var("HOST").required);
    assert_eq!(var("TOKEN").services, vec!["web"]);
}

#[test]
fn a_file_compose_rejects_is_rejected_in_composes_words() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "compose.yaml",
        "services:\n  a:\n    nonsense: 1\n",
    );
    let err = Project::open(
        dir.path().join("compose.yaml"),
        "app",
        Platform::new("linux", "amd64"),
        Vec::new(),
        "1",
    )
    .err()
    .unwrap()
    .to_string();
    assert!(err.contains("nonsense"), "{err}");
}
