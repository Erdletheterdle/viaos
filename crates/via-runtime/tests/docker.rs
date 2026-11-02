//! Against a real Docker daemon and the real `docker compose`, because "this
//! container was left alone" is theirs to claim. Ignored by default; it needs
//! the socket and an image that is already present:
//!
//! ```text
//! cargo test -p via-runtime --test docker --no-run
//! sudo VIA_TEST_IMAGE=ubuntu:24.04 target/debug/deps/docker-<hash> --ignored --test-threads=1
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use via_core::{Deployment, ImageRef, Platform, Service, VariableSpec, Volume};
use via_runtime::{ApplyContext, ContainerStatus, DockerRuntime, ExecOptions, Runtime};

fn image() -> String {
    std::env::var("VIA_TEST_IMAGE").unwrap_or_else(|_| "ubuntu:24.04".into())
}

fn deployment(project: &str, services: &[(&str, &[&str])]) -> Deployment {
    let mut d = Deployment::new(project, Platform::new("linux", "amd64"));
    d.health_gate.timeout_secs = 60;
    for (name, env) in services {
        let mut s = Service::new(*name, ImageRef::new(image()));
        s.runtime_env = env.iter().map(|k| k.to_string()).collect();
        d.services.insert(name.to_string(), s);
    }
    d
}

fn lay_out(dir: &Path, compose: &str) {
    std::fs::write(dir.join("compose.yaml"), compose.replace("IMAGE", &image())).unwrap();
}

fn context(dir: &Path, values: &[(&str, &str)]) -> ApplyContext {
    let mut ctx = ApplyContext::default().with_global(
        values
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    );
    ctx.project_dir = Some(dir.to_path_buf());
    ctx
}

async fn ids(rt: &DockerRuntime, d: &Deployment) -> BTreeMap<String, String> {
    rt.service_states(d)
        .await
        .unwrap()
        .into_iter()
        .map(|s| (s.service, s.container_id.unwrap()))
        .collect()
}

async fn output(rt: &DockerRuntime, d: &Deployment, service: &str, command: &[&str]) -> String {
    use futures::StreamExt;
    let exec = rt
        .exec(
            d,
            &ExecOptions {
                service: service.into(),
                command: command.iter().map(|c| c.to_string()).collect(),
            },
        )
        .await
        .unwrap();
    let mut text = String::new();
    let mut stream = exec.output;
    while let Some(chunk) = stream.next().await {
        if let via_runtime::ExecChunk::Output { data, .. } = chunk.unwrap() {
            text.push_str(&data);
        }
    }
    text
}

const STACK: &str = r#"
x-sleep: &sleep
  image: IMAGE
  command: ["sleep", "600"]
  init: true
services:
  db:
    <<: *sleep
  web:
    <<: *sleep
    depends_on: [db]
    environment:
      - API_KEY
      - LOG_LEVEL
  worker:
    <<: *sleep
    depends_on: [web]
    environment:
      - LOG_LEVEL
"#;

#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn changing_a_value_replaces_only_the_container_that_reads_it() {
    let dir = tempfile::tempdir().unwrap();
    lay_out(dir.path(), STACK);
    let rt = DockerRuntime::connect().unwrap();
    let d = deployment(
        "via-test-values",
        &[
            ("db", &[]),
            ("web", &["API_KEY", "LOG_LEVEL"]),
            ("worker", &["LOG_LEVEL"]),
        ],
    );

    let first = context(dir.path(), &[("API_KEY", "a$b"), ("LOG_LEVEL", "info")]);
    let report = rt.apply(&d, &first).await.unwrap();
    assert_eq!(report.created.len(), 3, "{report:?}");
    let start = ids(&rt, &d).await;
    assert_eq!(
        output(&rt, &d, "web", &["printenv", "API_KEY"])
            .await
            .trim(),
        "a$b",
        "a dollar sign in a value reaches the container as written"
    );

    let again = rt.apply(&d, &first).await.unwrap();
    assert!(!again.changed(), "{again:?}");
    assert_eq!(ids(&rt, &d).await, start);

    let rotated = context(dir.path(), &[("API_KEY", "new"), ("LOG_LEVEL", "info")]);
    rt.apply(&d, &rotated).await.unwrap();
    let after = ids(&rt, &d).await;
    assert_eq!(after["db"], start["db"], "db reads neither");
    assert_ne!(after["web"], start["web"], "web reads API_KEY");
    assert_eq!(
        output(&rt, &d, "web", &["printenv", "API_KEY"])
            .await
            .trim(),
        "new"
    );

    rt.remove(&d, true).await.unwrap();
    let gone = rt.service_states(&d).await.unwrap();
    assert!(gone.iter().all(|s| s.status == ContainerStatus::Absent));
}

#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn compose_features_we_never_wrote_just_work() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("conf")).unwrap();
    std::fs::write(dir.path().join("conf/app.conf"), "from the project\n").unwrap();
    std::fs::write(dir.path().join(".env"), "FROM_DOTENV=yes\n").unwrap();
    std::fs::write(dir.path().join("web.env"), "FROM_ENV_FILE=yes\n").unwrap();
    std::fs::write(dir.path().join("settings.ini"), "a config\n").unwrap();
    lay_out(
        dir.path(),
        r#"
services:
  job:
    image: IMAGE
    command: ["sh", "-c", "echo done > /shared/job"]
    volumes: [shared:/shared]
  web:
    image: IMAGE
    command: ["sleep", "600"]
    init: true
    env_file: web.env
    environment:
      DOTENV: ${FROM_DOTENV}
      PORT_IS: ${PORT:-8080}
      KEPT: $$LITERAL
    volumes:
      - ./conf:/etc/conf:ro
      - shared:/shared
    tmpfs: /scratch
    configs: [settings]
    cap_drop: [ALL]
    sysctls: {net.ipv4.ip_unprivileged_port_start: "1"}
    deploy: {replicas: 2}
    depends_on:
      job: {condition: service_completed_successfully}
  sidecar:
    image: IMAGE
    command: ["sleep", "600"]
    init: true
    network_mode: "service:db"
  db:
    image: IMAGE
    command: ["sleep", "600"]
    init: true
    environment: {GEN: "1"}
configs:
  settings: {file: ./settings.ini}
volumes:
  shared:
"#,
    );
    let rt = DockerRuntime::connect().unwrap();
    let mut d = deployment(
        "via-test-features",
        &[("job", &[]), ("web", &[]), ("sidecar", &[]), ("db", &[])],
    );
    d.volumes.insert(
        "shared".into(),
        Volume {
            name: Some("via-test-features_shared".into()),
            ..Default::default()
        },
    );

    let ctx = context(dir.path(), &[("PORT", "9090")]);
    rt.apply(&d, &ctx).await.unwrap();
    let env = output(&rt, &d, "web", &["env"]).await;
    assert!(env.contains("DOTENV=yes"), "{env}");
    assert!(env.contains("FROM_ENV_FILE=yes"), "{env}");
    assert!(env.contains("PORT_IS=9090"), "{env}");
    assert!(env.contains("KEPT=$LITERAL"), "{env}");
    assert_eq!(
        output(&rt, &d, "web", &["cat", "/etc/conf/app.conf"]).await,
        "from the project\n"
    );
    assert_eq!(
        output(&rt, &d, "web", &["cat", "/shared/job"]).await,
        "done\n"
    );
    assert_eq!(
        output(&rt, &d, "web", &["cat", "/settings"]).await,
        "a config\n"
    );

    // The bug the hand-rolled runtime had: replacing `db` has to take along
    // what shares its network.
    let sidecar = ids(&rt, &d).await["sidecar"].clone();
    let text = std::fs::read_to_string(dir.path().join("compose.yaml")).unwrap();
    std::fs::write(
        dir.path().join("compose.yaml"),
        text.replace("GEN: \"1\"", "GEN: \"2\""),
    )
    .unwrap();
    rt.apply(&d, &ctx).await.unwrap();
    assert_ne!(ids(&rt, &d).await["sidecar"], sidecar);
    assert!(
        !output(&rt, &d, "sidecar", &["true"])
            .await
            .contains("error")
    );

    // A removed service goes, as `--remove-orphans` promises.
    let text = std::fs::read_to_string(dir.path().join("compose.yaml")).unwrap();
    let cut = text.find("  sidecar:").unwrap()..text.find("  db:").unwrap();
    let mut shorter = text.clone();
    shorter.replace_range(cut, "");
    std::fs::write(dir.path().join("compose.yaml"), shorter).unwrap();
    let report = rt.apply(&d, &ctx).await.unwrap();
    assert_eq!(report.removed_orphans, vec!["sidecar"], "{report:?}");

    rt.remove(&d, true).await.unwrap();
}

#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn a_file_compose_refuses_says_so_in_composes_words() {
    let dir = tempfile::tempdir().unwrap();
    lay_out(
        dir.path(),
        "services:\n  a:\n    image: IMAGE\n    environment:\n      X: ${MISSING:?set MISSING first}\n",
    );
    let rt = DockerRuntime::connect().unwrap();
    let d = deployment("via-test-refuses", &[("a", &[])]);
    let err = rt
        .apply(&d, &context(dir.path(), &[]))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("set MISSING first"), "{err}");
}

#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn a_default_from_the_release_reaches_a_service_that_asked() {
    let dir = tempfile::tempdir().unwrap();
    lay_out(
        dir.path(),
        "services:\n  a:\n    image: IMAGE\n    command: [sleep, '600']\n    init: true\n    environment: [GREETING]\n",
    );
    let rt = DockerRuntime::connect().unwrap();
    let mut d = deployment("via-test-default", &[("a", &["GREETING"])]);
    d.variables.push(VariableSpec {
        key: "GREETING".into(),
        services: vec!["a".into()],
        description: None,
        default: Some("hi".into()),
        secret: false,
        required: false,
    });
    rt.apply(&d, &context(dir.path(), &[])).await.unwrap();
    assert_eq!(
        output(&rt, &d, "a", &["printenv", "GREETING"]).await.trim(),
        "hi"
    );
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(dir.path().join(via_runtime::OVERRIDE_FILE))
            .unwrap()
            .permissions(),
    );
    assert_eq!(mode & 0o777, 0o600, "it can hold secrets");
    rt.remove(&d, true).await.unwrap();
}

#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn what_viaos_started_before_compose_gives_way_and_its_data_stays() {
    use bollard::models::{ContainerCreateBody, HostConfig, NetworkCreateRequest};
    use bollard::query_parameters as qp;

    let project = "via-test-legacy";
    let rt = DockerRuntime::connect().unwrap();
    let docker = rt.client();
    let label = std::collections::HashMap::from([("via.project".to_string(), project.to_string())]);

    docker
        .create_network(NetworkCreateRequest {
            name: format!("{project}_default"),
            labels: Some(label.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    docker
        .create_volume(bollard::models::VolumeCreateRequest {
            name: Some(format!("{project}_data")),
            labels: Some(label.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    let name = format!("{project}-web");
    docker
        .create_container(
            Some(qp::CreateContainerOptionsBuilder::new().name(&name).build()),
            ContainerCreateBody {
                image: Some(image()),
                cmd: Some(vec![
                    "sh".into(),
                    "-c".into(),
                    "echo kept > /data/file; sleep 600".into(),
                ]),
                labels: Some(label),
                host_config: Some(HostConfig {
                    binds: Some(vec![format!("{project}_data:/data")]),
                    init: Some(true),
                    network_mode: Some(format!("{project}_default")),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    docker
        .start_container(&name, None::<qp::StartContainerOptions>)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    let dir = tempfile::tempdir().unwrap();
    lay_out(
        dir.path(),
        "services:\n  web:\n    image: IMAGE\n    command: [sleep, '600']\n    init: true\n    volumes: [data:/data]\nvolumes:\n  data:\n",
    );
    let d = deployment(project, &[("web", &[])]);
    rt.apply(&d, &context(dir.path(), &[])).await.unwrap();

    assert_eq!(
        output(&rt, &d, "web", &["cat", "/data/file"]).await,
        "kept\n"
    );
    let old = docker
        .inspect_container(&name, None::<qp::InspectContainerOptions>)
        .await;
    assert!(old.is_err(), "the old container is gone");

    rt.remove(&d, true).await.unwrap();
    let _ = docker
        .remove_volume(&format!("{project}_data"), None::<qp::RemoveVolumeOptions>)
        .await;
}
