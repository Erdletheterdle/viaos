//! Normalization tests, written against the shorthands real compose files use.

use via_compose::{Error, NormalizeOptions, Normalized, normalize, parse_str};
use via_core::{Mount, Platform, Protocol, Readiness, RestartPolicy};

fn norm(yaml: &str) -> Result<Normalized, Error> {
    let file = parse_str(yaml)?;
    let opts = NormalizeOptions::new("demo-stack", Platform::parse("linux/arm64").unwrap())
        .build_tag("44")
        .base_dir("/src/app");
    normalize(&file, &opts)
}

fn ok(yaml: &str) -> Normalized {
    norm(yaml).unwrap_or_else(|e| panic!("expected normalization to succeed: {e}"))
}

#[test]
fn minimal_compose_file_normalizes() {
    let n = ok(r#"
services:
  app:
    image: nginx:1.27
"#);
    assert_eq!(n.deployment.services.len(), 1);
    assert_eq!(n.deployment.project, "demo-stack");
    assert_eq!(n.deployment.services["app"].image.reference, "nginx:1.27");
    assert!(n.builds.is_empty());
}

#[test]
fn build_services_get_a_release_scoped_tag_not_latest() {
    // Two releases must not produce colliding image tags, or a rollback would
    // find the wrong image already loaded under the same name.
    let n = ok(r#"
services:
  detector:
    build: ./detector
"#);
    assert_eq!(
        n.deployment.services["detector"].image.reference,
        "demo-stack/detector:44"
    );
    assert_eq!(n.builds.len(), 1);
    assert_eq!(n.builds[0].service, "detector");
    assert_eq!(n.builds[0].context.to_string_lossy(), "/src/app/detector");
}

#[test]
fn explicit_image_wins_as_the_build_tag() {
    let n = ok(r#"
services:
  detector:
    build: ./detector
    image: registry.example.com/detector:1.2.3
"#);
    assert_eq!(n.builds[0].tag, "registry.example.com/detector:1.2.3");
    assert_eq!(
        n.deployment.services["detector"].image.reference,
        "registry.example.com/detector:1.2.3"
    );
}

#[test]
fn service_with_neither_image_nor_build_is_rejected() {
    let err = norm("services:\n  app:\n    command: true\n").unwrap_err();
    assert!(matches!(err, Error::ServiceHasNoImage { .. }), "{err}");
}

#[test]
fn command_string_is_shell_split_not_wrapped_in_sh() {
    // Compose word-splits a string command. Wrapping it in `sh -c` would
    // change the process tree and break signal handling.
    let n = ok(r#"
services:
  app:
    image: busybox
    command: echo "hello world" plain
"#);
    assert_eq!(
        n.deployment.services["app"].command.as_deref().unwrap(),
        ["echo", "hello world", "plain"]
    );
}

#[test]
fn command_list_is_taken_verbatim() {
    let n = ok(r#"
services:
  app:
    image: busybox
    command: ["sleep", "3600"]
"#);
    assert_eq!(
        n.deployment.services["app"].command.as_deref().unwrap(),
        ["sleep", "3600"]
    );
}

#[test]
fn environment_accepts_map_and_list_and_coerces_scalars() {
    let n = ok(r#"
services:
  a:
    image: busybox
    environment:
      PORT: 8080
      DEBUG: true
      NAME: hello
  b:
    image: busybox
    environment:
      - PORT=9090
      - NAME=world
"#);
    let a = &n.deployment.services["a"].environment;
    assert_eq!(a["PORT"], "8080", "a YAML integer must survive as a string");
    assert_eq!(a["DEBUG"], "true");
    assert_eq!(n.deployment.services["b"].environment["PORT"], "9090");
}

#[test]
fn short_volume_forms_distinguish_named_volumes_from_binds() {
    let n = ok(r#"
services:
  db:
    image: postgres:17
    volumes:
      - db-data:/var/lib/postgresql/data
      - ./config:/etc/app:ro
      - /var/run/docker.sock:/var/run/docker.sock
volumes:
  db-data:
"#);
    let mounts = &n.deployment.services["db"].mounts;
    assert!(
        matches!(&mounts[0], Mount::Volume { source, read_only: false, .. } if source == "db-data")
    );
    assert!(
        matches!(&mounts[1], Mount::Bind { source, read_only: true, .. } if source == "/src/app/config"),
        "relative binds must be made absolute against the compose directory: {:?}",
        mounts[1]
    );
    assert!(matches!(&mounts[2], Mount::Bind { source, .. } if source == "/var/run/docker.sock"));
}

#[test]
fn anonymous_volumes_get_a_stable_derived_name() {
    // Docker would assign a random name, orphaning the data on every redeploy.
    let n = ok(r#"
services:
  db:
    image: postgres:17
    volumes:
      - /var/lib/postgresql/data
"#);
    let Mount::Volume { source, .. } = &n.deployment.services["db"].mounts[0] else {
        panic!("expected a named volume");
    };
    assert_eq!(source, "demo-stack-db-var-lib-postgresql-data");
}

#[test]
fn duplicate_mount_targets_are_rejected() {
    let err = norm(
        r#"
services:
  db:
    image: postgres:17
    volumes:
      - a:/data
      - b:/data
volumes:
  a:
  b:
"#,
    )
    .unwrap_err();
    assert!(matches!(err, Error::InvalidMount { .. }), "{err}");
}

#[test]
fn port_short_forms_cover_ip_protocol_and_ephemeral() {
    let n = ok(r#"
services:
  app:
    image: busybox
    ports:
      - "8080:80"
      - "127.0.0.1:5432:5432"
      - "1234:1234/udp"
      - "80"
"#);
    let p = &n.deployment.services["app"].ports;
    assert_eq!((p[0].host_port, p[0].container_port), (Some(8080), 80));
    assert_eq!(p[1].host_ip.as_deref(), Some("127.0.0.1"));
    assert_eq!(p[2].protocol, Protocol::Udp);
    assert_eq!(
        p[3].host_port, None,
        "a bare port publishes on an ephemeral host port"
    );
    assert_eq!(p[3].container_port, 80);
}

#[test]
fn port_ranges_expand_and_mismatched_lengths_are_rejected() {
    let n = ok(r#"
services:
  app:
    image: busybox
    ports:
      - "3000-3002:3000-3002"
"#);
    assert_eq!(n.deployment.services["app"].ports.len(), 3);

    let err = norm(
        r#"
services:
  app:
    image: busybox
    ports:
      - "3000-3002:4000-4009"
"#,
    )
    .unwrap_err();
    assert!(matches!(err, Error::InvalidPort { .. }), "{err}");
}

#[test]
fn restart_defaults_to_unless_stopped_not_no() {
    // An unattended device must bring its application back after a power cut.
    let n = ok("services:\n  app:\n    image: busybox\n");
    assert_eq!(
        n.deployment.services["app"].restart,
        RestartPolicy::UnlessStopped
    );
}

#[test]
fn restart_on_failure_keeps_its_retry_count() {
    let n = ok(r#"
services:
  app:
    image: busybox
    restart: on-failure:5
"#);
    assert_eq!(
        n.deployment.services["app"].restart,
        RestartPolicy::OnFailure {
            max_retries: Some(5)
        }
    );
}

#[test]
fn healthcheck_string_becomes_cmd_shell_and_durations_parse() {
    let n = ok(r#"
services:
  app:
    image: busybox
    healthcheck:
      test: curl -f http://localhost/health
      interval: 1m30s
      start_period: 500ms
      retries: 3
"#);
    let hc = n.deployment.services["app"].healthcheck.as_ref().unwrap();
    assert_eq!(hc.test[0], "CMD-SHELL");
    assert_eq!(hc.interval_secs, Some(90));
    assert_eq!(
        hc.start_period_secs,
        Some(1),
        "sub-second rounds up, never to zero"
    );
    assert_eq!(hc.retries, Some(3));
}

#[test]
fn disabled_healthcheck_is_treated_as_absent() {
    let n = ok(r#"
services:
  app:
    image: busybox
    healthcheck:
      disable: true
"#);
    assert!(n.deployment.services["app"].healthcheck.is_none());
    assert_eq!(n.deployment.services["app"].readiness, Readiness::Running);
}

#[test]
fn readiness_defaults_to_healthy_when_a_healthcheck_exists() {
    let n = ok(r#"
services:
  app:
    image: busybox
    healthcheck:
      test: ["CMD", "true"]
"#);
    assert_eq!(n.deployment.services["app"].readiness, Readiness::Healthy);
    assert!(
        n.warnings.is_empty(),
        "a service with a healthcheck should not warn"
    );
}

#[test]
fn readiness_defaults_to_running_and_warns_when_no_healthcheck_exists() {
    let n = ok("services:\n  app:\n    image: busybox\n");
    assert_eq!(n.deployment.services["app"].readiness, Readiness::Running);
    assert!(
        n.warnings.iter().any(|w| w.message.contains("healthcheck")),
        "the operator should be told the gate is weak: {:?}",
        n.warnings
    );
}

#[test]
fn readiness_healthy_without_a_healthcheck_is_rejected_at_build_time() {
    // Otherwise every deploy of this release would sit in HEALTH CHECK until
    // the gate timed out, on every device in the fleet.
    let err = norm(
        r#"
services:
  app:
    image: busybox
    x-via:
      readiness: healthy
"#,
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("never report healthy"), "{msg}");
}

#[test]
fn readiness_can_be_set_explicitly() {
    let n = ok(r#"
services:
  migrate:
    image: busybox
    x-via:
      readiness: completed
  flaky:
    image: busybox
    x-via:
      readiness: ignored
"#);
    assert_eq!(
        n.deployment.services["migrate"].readiness,
        Readiness::Completed
    );
    assert_eq!(n.deployment.services["flaky"].readiness, Readiness::Ignored);
}

#[test]
fn depends_on_accepts_list_and_condition_map() {
    use via_core::DependencyCondition;
    let n = ok(r#"
services:
  app:
    image: busybox
    depends_on:
      db:
        condition: service_healthy
  worker:
    image: busybox
    depends_on: [db]
  db:
    image: postgres:17
    healthcheck:
      test: ["CMD", "pg_isready"]
"#);
    assert_eq!(
        n.deployment.services["app"].depends_on[0].condition,
        DependencyCondition::Healthy
    );
    assert_eq!(
        n.deployment.services["worker"].depends_on[0].condition,
        DependencyCondition::Started
    );

    let order = n.deployment.start_order().unwrap();
    let pos = |s: &str| order.iter().position(|x| *x == s).unwrap();
    assert!(pos("db") < pos("app"));
}

#[test]
fn dangling_references_fail_at_build_time_not_on_the_device() {
    let cases = [
        (
            "depends_on",
            "services:\n  app:\n    image: b\n    depends_on: [nope]\n",
        ),
        (
            "network",
            "services:\n  app:\n    image: b\n    networks: [nope]\n",
        ),
        (
            "volume",
            "services:\n  app:\n    image: b\n    volumes:\n      - nope:/data\n",
        ),
    ];
    for (label, yaml) in cases {
        let err = norm(yaml).unwrap_err();
        assert!(
            matches!(err, Error::UnknownReference { .. }),
            "{label}: {err}"
        );
    }
}

#[test]
fn dependency_cycles_are_reported_with_the_path() {
    let err = norm(
        r#"
services:
  a:
    image: b
    depends_on: [b]
  b:
    image: b
    depends_on: [a]
"#,
    )
    .unwrap_err();
    assert!(matches!(err, Error::DependencyCycle { .. }), "{err}");
}

#[test]
fn devices_and_capabilities_survive() {
    // The hardware attached to the device is usually the entire point.
    let n = ok(r#"
services:
  detector:
    image: busybox
    privileged: true
    devices:
      - /dev/video0
      - /dev/ttyUSB0:/dev/plc:rw
    cap_add: [SYS_RAWIO]
    group_add: ["44"]
"#);
    let s = &n.deployment.services["detector"];
    assert!(s.privileged);
    assert_eq!(s.devices[0].host_path, "/dev/video0");
    assert_eq!(s.devices[0].effective_container_path(), "/dev/video0");
    assert_eq!(s.devices[1].effective_container_path(), "/dev/plc");
    assert_eq!(s.devices[1].permissions, "rw");
    assert_eq!(s.cap_add, ["SYS_RAWIO"]);
    assert_eq!(s.group_add, ["44"]);
}

#[test]
fn network_mode_host_is_preserved() {
    let n = ok(r#"
services:
  app:
    image: busybox
    network_mode: host
"#);
    assert_eq!(
        n.deployment.services["app"].network_mode.as_deref(),
        Some("host")
    );
}

#[test]
fn multiple_replicas_are_rejected_rather_than_silently_ignored() {
    let err = norm(
        r#"
services:
  app:
    image: busybox
    deploy:
      replicas: 3
"#,
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("replicas"), "{msg}");
}

#[test]
fn inactive_profiles_omit_services() {
    let n = ok(r#"
services:
  app:
    image: busybox
  debug-tools:
    image: busybox
    profiles: [debug]
"#);
    assert!(!n.deployment.services.contains_key("debug-tools"));
    assert!(n.deployment.services.contains_key("app"));
}

#[test]
fn obsolete_version_key_warns_but_still_parses() {
    let n = ok(r#"
version: "3.8"
services:
  app:
    image: busybox
"#);
    assert!(n.warnings.iter().any(|w| w.message.contains("version")));
}

#[test]
fn a_typo_in_a_top_level_key_is_an_error() {
    // `deny_unknown_fields` is what turns `servics:` into a build failure
    // instead of an empty deployment.
    let err = norm("servics:\n  app:\n    image: busybox\n").unwrap_err();
    assert!(
        matches!(err, Error::Yaml { .. } | Error::NoServices),
        "{err}"
    );
}

#[test]
fn realistic_stack_normalizes_end_to_end() {
    let n = ok(r#"
services:
  detector:
    build:
      context: ./detector
      dockerfile: Dockerfile.arm
      args:
        MODEL: yolov8n
    devices:
      - /dev/video0
    depends_on:
      db:
        condition: service_healthy
      nats:
        condition: service_started
    environment:
      SAMPLE_RATE: 10
    restart: unless-stopped

  dashboard:
    build: ./dashboard
    ports:
      - "8080:80"
    depends_on: [detector]

  db:
    image: postgres:17
    volumes:
      - db-data:/var/lib/postgresql/data
    environment:
      POSTGRES_PASSWORD: unused-placeholder
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U postgres"]
      interval: 10s
      retries: 5

  nats:
    image: nats:2

volumes:
  db-data:
"#);

    assert_eq!(n.deployment.services.len(), 4);
    assert_eq!(n.builds.len(), 2, "detector and dashboard need building");
    assert_eq!(n.deployment.required_images().len(), 4);

    let order = n.deployment.start_order().unwrap();
    let pos = |s: &str| order.iter().position(|x| *x == s).unwrap();
    assert!(pos("db") < pos("detector"));
    assert!(pos("nats") < pos("detector"));
    assert!(pos("detector") < pos("dashboard"));

    assert_eq!(n.deployment.services["db"].readiness, Readiness::Healthy);
    assert_eq!(n.deployment.services["nats"].readiness, Readiness::Running);

    let build = n.builds.iter().find(|b| b.service == "detector").unwrap();
    assert_eq!(build.dockerfile.as_deref(), Some("Dockerfile.arm"));
    assert_eq!(build.args["MODEL"], "yolov8n");

    // The manifest must survive the trip to the device unchanged.
    let json = serde_json::to_string(&n.deployment).unwrap();
    let back: via_core::Deployment = serde_json::from_str(&json).unwrap();
    assert_eq!(back.services.len(), 4);
}

#[test]
fn a_bare_environment_key_is_a_runtime_input_not_a_build_host_lookup() {
    // Compose's pass-through form. Resolving it from the build machine would
    // bake a developer's shell into the release; recording it lets the agent
    // deliver a variable or secret to exactly the services that asked.
    unsafe { std::env::set_var("VIA_TEST_SHOULD_NOT_LEAK", "from-the-build-host") };
    let n = ok(r#"
services:
  detector:
    image: busybox
    environment:
      - CLOUD_API_TOKEN
      - VIA_TEST_SHOULD_NOT_LEAK
      - SAMPLE_RATE=10
  db:
    image: postgres:17
"#);
    let detector = &n.deployment.services["detector"];
    assert_eq!(detector.environment["SAMPLE_RATE"], "10");
    assert!(
        !detector.environment.contains_key("CLOUD_API_TOKEN"),
        "an undefined key has no value yet"
    );
    assert!(
        !detector
            .environment
            .contains_key("VIA_TEST_SHOULD_NOT_LEAK"),
        "the build host's environment must not reach the release"
    );
    assert_eq!(
        detector.runtime_env,
        ["CLOUD_API_TOKEN", "VIA_TEST_SHOULD_NOT_LEAK"]
    );

    assert!(detector.wants_runtime_key("CLOUD_API_TOKEN"));
    assert!(!n.deployment.services["db"].wants_runtime_key("CLOUD_API_TOKEN"));
}

#[test]
fn a_mapping_key_with_no_value_is_also_a_runtime_input() {
    let n = ok(r#"
services:
  app:
    image: busybox
    environment:
      CLOUD_API_TOKEN:
      SAMPLE_RATE: 10
"#);
    assert_eq!(
        n.deployment.services["app"].runtime_env,
        ["CLOUD_API_TOKEN"]
    );
    assert_eq!(
        n.deployment.services["app"].environment["SAMPLE_RATE"],
        "10"
    );
}
