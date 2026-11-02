//! What a release says about itself, read off `docker compose config`.
//!
//! None of this decides how anything runs. It is what the console shows, which
//! images a bundle carries, and what the health gate waits for.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use via_core::{
    Dependency, DependencyCondition, Deployment, HealthCheck, HealthGate, ImageRef, Mount, Network,
    Platform, PortMapping, Protocol, Readiness, RestartPolicy, Service, VariableSpec, Volume,
};

use crate::{Error, Warning};

/// One `$NAME` or `${NAME...}` in a compose file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableRef {
    pub name: String,
    pub default: Option<String>,
    /// `${NAME:?message}`: Compose refuses to run without it.
    pub required: bool,
}

/// Every variable a value mentions, in order, wherever in it they are.
pub fn references(value: &Value) -> Vec<VariableRef> {
    let mut out = Vec::new();
    walk(value, &mut out);
    out
}

fn walk(value: &Value, out: &mut Vec<VariableRef>) {
    match value {
        Value::String(s) => scan(s, out),
        Value::Array(a) => a.iter().for_each(|v| walk(v, out)),
        Value::Object(o) => {
            for (k, v) in o {
                if !k.starts_with("x-") {
                    walk(v, out);
                }
            }
        }
        _ => {}
    }
}

/// Compose's own interpolation forms, read only far enough to name them.
fn scan(text: &str, out: &mut Vec<VariableRef>) {
    let chars: Vec<char> = text.chars().collect();
    let name_char = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '$' {
            i += 1;
            continue;
        }
        match chars.get(i + 1) {
            Some('$') => i += 2,
            Some('{') => {
                let mut depth = 1;
                let mut end = i + 2;
                while end < chars.len() && depth > 0 {
                    match chars[end] {
                        '{' => depth += 1,
                        '}' => depth -= 1,
                        _ => {}
                    }
                    end += 1;
                }
                let inner: String = chars[i + 2..end.saturating_sub(1).max(i + 2)]
                    .iter()
                    .collect();
                let split = inner.find(|c: char| !name_char(c)).unwrap_or(inner.len());
                let (name, rest) = inner.split_at(split);
                if !name.is_empty() {
                    let rest = rest.strip_prefix(':').unwrap_or(rest);
                    let (default, required) = match rest.chars().next() {
                        Some('-') => (Some(rest[1..].to_string()), false),
                        Some('?') => (None, true),
                        _ => (None, false),
                    };
                    out.push(VariableRef {
                        name: name.to_string(),
                        default,
                        required,
                    });
                }
                // A default can name another variable.
                if let Some(pos) = inner.find('$') {
                    scan(&inner[pos..], out);
                }
                i = end;
            }
            Some(c) if c.is_ascii_alphabetic() || *c == '_' => {
                let mut end = i + 1;
                while end < chars.len() && name_char(chars[end]) {
                    end += 1;
                }
                out.push(VariableRef {
                    name: chars[i + 1..end].iter().collect(),
                    default: None,
                    required: false,
                });
                i = end;
            }
            _ => i += 1,
        }
    }
}

fn warn(service: &str, message: impl Into<String>) -> Warning {
    Warning {
        service: Some(service.to_string()),
        message: message.into(),
    }
}

/// Environment entries in either of Compose's forms, as name and raw value.
fn environment(value: &Value) -> BTreeMap<String, Option<String>> {
    let text = |v: &Value| match v {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    };
    match value {
        Value::Object(o) => o.iter().map(|(k, v)| (k.clone(), text(v))).collect(),
        Value::Array(a) => a
            .iter()
            .filter_map(Value::as_str)
            .map(|e| match e.split_once('=') {
                Some((k, v)) => (k.to_string(), Some(v.to_string())),
                None => (e.to_string(), None),
            })
            .collect(),
        _ => BTreeMap::new(),
    }
}

pub(crate) fn deployment(
    project: &str,
    platform: Platform,
    flat: &Value,
    resolved: &Value,
    dotenv: &BTreeSet<String>,
) -> Result<(Deployment, Vec<Warning>), Error> {
    let mut d = Deployment::new(project, platform);
    d.health_gate = HealthGate::default();
    let mut warnings = Vec::new();

    let empty = serde_json::Map::new();
    let services = resolved["services"].as_object().unwrap_or(&empty);

    // A job something waits to finish is meant to exit.
    let jobs: BTreeSet<&str> = services
        .values()
        .flat_map(|s| s["depends_on"].as_object().into_iter().flatten())
        .filter(|(_, dep)| dep["condition"] == "service_completed_successfully")
        .map(|(name, _)| name.as_str())
        .collect();

    for (name, r) in services {
        let f = &flat["services"][name];
        let image = r["image"].as_str().ok_or_else(|| Error::Service {
            service: name.clone(),
            reason: "has no image".into(),
        })?;
        let mut s = Service::new(name, ImageRef::new(image));

        let raw_env = environment(&f["environment"]);
        for (key, value) in environment(&r["environment"]) {
            let from_device = raw_env
                .get(&key)
                .and_then(Option::as_ref)
                .is_some_and(|raw| !references(&Value::String(raw.clone())).is_empty());
            match value {
                None => s.runtime_env.push(key),
                Some(_) if from_device => {}
                Some(value) => {
                    // `config` keeps `$$` so its output can be read back.
                    s.environment.insert(key, value.replace("$$", "$"));
                }
            }
        }
        s.runtime_env.sort();

        s.mounts = f["volumes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(mount)
            .collect();
        s.ports = r["ports"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(port)
            .collect();
        s.networks = r["networks"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, _)| k.clone())
            .collect();
        s.network_mode = r["network_mode"].as_str().map(str::to_string);
        s.privileged = r["privileged"].as_bool().unwrap_or(false);
        s.depends_on = r["depends_on"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(service, dep)| Dependency {
                service: service.clone(),
                condition: match dep["condition"].as_str() {
                    Some("service_healthy") => DependencyCondition::Healthy,
                    Some("service_completed_successfully") => {
                        DependencyCondition::CompletedSuccessfully
                    }
                    _ => DependencyCondition::Started,
                },
            })
            .collect();
        if r["healthcheck"]["disable"] != Value::Bool(true) {
            if let Some(test) = r["healthcheck"]["test"].as_array() {
                s.healthcheck = Some(HealthCheck {
                    test: test
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect(),
                    interval_secs: None,
                    timeout_secs: None,
                    retries: None,
                    start_period_secs: None,
                });
            }
        }
        s.restart = match r["restart"].as_str() {
            Some("always") => RestartPolicy::Always,
            Some("unless-stopped") => RestartPolicy::UnlessStopped,
            Some(p) if p.starts_with("on-failure") => RestartPolicy::OnFailure {
                max_retries: p.split_once(':').and_then(|(_, n)| n.parse().ok()),
            },
            _ => {
                let job = jobs.contains(name.as_str());
                if !job && r["deploy"]["restart_policy"].is_null() {
                    warnings.push(warn(
                        name,
                        "no `restart:`, so Docker won't restart it if it crashes",
                    ));
                }
                RestartPolicy::No
            }
        };
        s.replicas = r["scale"]
            .as_u64()
            .or(r["deploy"]["replicas"].as_u64())
            .unwrap_or(1) as u32;
        s.readiness = readiness(name, f, &s, jobs.contains(name.as_str()), &mut warnings)?;
        d.services.insert(name.clone(), s);
    }

    for (name, network) in resolved["networks"].as_object().into_iter().flatten() {
        d.networks.insert(
            name.clone(),
            Network {
                external: network["external"].as_bool().unwrap_or(false),
                internal: network["internal"].as_bool().unwrap_or(false),
                driver: network["driver"].as_str().map(str::to_string),
                ..Default::default()
            },
        );
    }
    for (name, volume) in resolved["volumes"].as_object().into_iter().flatten() {
        d.volumes.insert(
            name.clone(),
            Volume {
                name: volume["name"].as_str().map(str::to_string),
                external: volume["external"].as_bool().unwrap_or(false),
                driver: volume["driver"].as_str().map(str::to_string),
                ..Default::default()
            },
        );
    }

    d.variables = variables(flat, &mut d.services, dotenv, &mut warnings)?;
    Ok((d, warnings))
}

fn mount(value: &Value) -> Option<Mount> {
    let target = value["target"].as_str()?.to_string();
    let read_only = value["read_only"].as_bool().unwrap_or(false);
    let source = value["source"].as_str().map(str::to_string);
    match value["type"].as_str()? {
        "volume" => Some(match source {
            Some(source) => Mount::Volume {
                source,
                target,
                read_only,
            },
            // Anonymous: Docker makes one, and it has no name to size or show.
            None => Mount::Tmpfs {
                target,
                size_bytes: None,
            },
        }),
        "bind" => Some(Mount::Bind {
            source: source?,
            target,
            read_only,
        }),
        "tmpfs" => Some(Mount::Tmpfs {
            target,
            size_bytes: value["tmpfs"]["size"].as_u64(),
        }),
        _ => None,
    }
}

fn port(value: &Value) -> Option<PortMapping> {
    Some(PortMapping {
        host_ip: value["host_ip"].as_str().map(str::to_string),
        host_port: value["published"]
            .as_str()
            .and_then(|p| p.parse().ok())
            .or(value["published"].as_u64().map(|p| p as u16)),
        container_port: value["target"].as_u64()? as u16,
        protocol: match value["protocol"].as_str() {
            Some("udp") => Protocol::Udp,
            _ => Protocol::Tcp,
        },
    })
}

fn readiness(
    name: &str,
    flat: &Value,
    service: &Service,
    job: bool,
    warnings: &mut Vec<Warning>,
) -> Result<Readiness, Error> {
    let has_check = service.healthcheck.is_some();
    if let Some(explicit) = flat["x-via"]["readiness"].as_str() {
        let invalid = |reason: &str| Error::Service {
            service: name.to_string(),
            reason: format!("x-via.readiness = {explicit:?}: {reason}"),
        };
        return match explicit {
            "healthy" if !has_check => Err(invalid(
                "it has no healthcheck, so it could never report healthy",
            )),
            "healthy" => Ok(Readiness::Healthy),
            "running" => Ok(Readiness::Running),
            "finished" => Ok(Readiness::RunningOrFinished),
            "completed" => Ok(Readiness::Completed),
            "ignored" => Ok(Readiness::Ignored),
            _ => Err(invalid(
                "expected healthy, running, finished, completed or ignored",
            )),
        };
    }
    if service.replicas == 0 {
        return Ok(Readiness::Ignored);
    }
    if job {
        return Ok(Readiness::Completed);
    }
    if has_check {
        return Ok(Readiness::Healthy);
    }
    warnings.push(warn(
        name,
        "no healthcheck, so the deploy gate can only check that it starts and stays up",
    ));
    Ok(if service.restart == RestartPolicy::No {
        Readiness::RunningOrFinished
    } else {
        Readiness::Running
    })
}

/// What the release takes: what `x-via` describes, and whatever a service's
/// configuration interpolates. A bare `- KEY` is on the service already.
fn variables(
    flat: &Value,
    services: &mut BTreeMap<String, Service>,
    dotenv: &BTreeSet<String>,
    warnings: &mut Vec<Warning>,
) -> Result<Vec<VariableSpec>, Error> {
    let mut specs: BTreeMap<String, VariableSpec> = BTreeMap::new();

    for (name, service) in flat["services"].as_object().into_iter().flatten() {
        for r in references(service) {
            let in_dotenv = dotenv.contains(&r.name);
            let spec = specs.entry(r.name.clone()).or_insert_with(|| VariableSpec {
                key: r.name.clone(),
                services: Vec::new(),
                description: None,
                default: r.default.clone(),
                secret: false,
                required: r.default.is_none() && !in_dotenv,
            });
            if !spec.services.contains(name) {
                spec.services.push(name.clone());
            }
        }
    }

    for (key, raw) in flat["x-via"]["variables"].as_object().into_iter().flatten() {
        let invalid = |reason: String| Error::Variable {
            key: key.clone(),
            reason,
        };
        let mut chars = key.chars();
        let well_formed = chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !well_formed {
            return Err(invalid("not a variable name".into()));
        }
        let default = match &raw["default"] {
            Value::Null => None,
            Value::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        };
        let secret = raw["secret"].as_bool().unwrap_or(false);
        if secret && default.is_some() {
            return Err(invalid(
                "a secret cannot have a default: a release holds no secrets".into(),
            ));
        }
        // Naming a service is the same as that service listing a bare `- KEY`.
        for name in raw["services"].as_array().into_iter().flatten() {
            let name = name.as_str().unwrap_or_default();
            match services.get_mut(name) {
                Some(service) if !service.wants_runtime_key(key) => {
                    service.runtime_env.push(key.clone());
                    service.runtime_env.sort();
                }
                Some(_) => {}
                // Left out by its profile, or never there.
                None if flat["services"].get(name).is_none() => {
                    warnings.push(Warning {
                        service: None,
                        message: format!("x-via variable {key:?} names no service {name:?}"),
                    });
                }
                None => {}
            }
        }
        let spec = specs.entry(key.clone()).or_insert_with(|| VariableSpec {
            key: key.clone(),
            services: Vec::new(),
            description: None,
            default: None,
            secret: false,
            required: true,
        });
        for (name, service) in services.iter() {
            if service.wants_runtime_key(key) && !spec.services.contains(name) {
                spec.services.push(name.clone());
            }
        }
        if default.is_some() {
            spec.default = default;
        }
        spec.description = raw["description"].as_str().map(str::to_string);
        spec.secret = secret;
        spec.required = raw["required"]
            .as_bool()
            .unwrap_or(spec.default.is_none() && !dotenv.contains(key));
        if spec.services.is_empty() {
            warnings.push(Warning {
                service: None,
                message: format!(
                    "x-via variable {key:?} reaches no service: use it in a service's \
                     configuration, or name services in `services:`"
                ),
            });
        }
    }

    let mut out: Vec<VariableSpec> = specs.into_values().collect();
    for spec in &mut out {
        spec.services.sort();
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn names(text: &str) -> Vec<VariableRef> {
        references(&json!(text))
    }

    #[test]
    fn reads_every_form_compose_interpolates() {
        let r = names("$A ${B} ${C:-one} ${D-two} ${E:?why} ${F?why} ${G:+x}");
        let found: Vec<(&str, Option<&str>, bool)> = r
            .iter()
            .map(|r| (r.name.as_str(), r.default.as_deref(), r.required))
            .collect();
        assert_eq!(
            found,
            vec![
                ("A", None, false),
                ("B", None, false),
                ("C", Some("one"), false),
                ("D", Some("two"), false),
                ("E", None, true),
                ("F", None, true),
                ("G", None, false),
            ]
        );
    }

    #[test]
    fn an_escaped_dollar_is_not_a_variable() {
        assert!(names("echo $$HOME and $${HOME}").is_empty());
    }

    #[test]
    fn a_default_can_name_another_variable() {
        let r = names("${A:-${B:-x}}");
        assert_eq!(r[0].name, "A");
        assert_eq!(r[1].name, "B");
    }

    #[test]
    fn extensions_are_not_configuration() {
        assert!(references(&json!({"x-via": {"note": "$A"}})).is_empty());
    }
}
