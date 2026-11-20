//! Bind a full distribution to the application compiled into its launcher.
//! Inspect bytes rather than executing an arbitrary packaging input.
use anyhow::{bail, Context, Result};
use std::path::Path;

const BEGIN: &[u8] = b"\0pyappify.embedded-yaml.v1.begin\0";
const END: &[u8] = b"\0pyappify.embedded-yaml.v1.end\0";
static EMBEDDED: &str = concat!(
    "\0pyappify.embedded-yaml.v1.begin\0",
    include_str!("../../assets/pyappify.yml"),
    "\0pyappify.embedded-yaml.v1.end\0"
);

pub(crate) fn embedded_name() -> Result<String> {
    let config: serde_yaml::Value =
        serde_yaml::from_str(&EMBEDDED[BEGIN.len()..EMBEDDED.len() - END.len()])?;
    config
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .context("Missing embedded application name")
}

fn name_from_bytes(bytes: &[u8]) -> Result<String> {
    let mut identity: Option<String> = None;
    for (index, _) in bytes
        .windows(BEGIN.len())
        .enumerate()
        .filter(|(_, value)| *value == BEGIN)
    {
        let yaml = &bytes[index + BEGIN.len()..];
        let Some(end) = yaml.windows(END.len()).position(|value| value == END) else {
            continue;
        };
        let Ok(config) = serde_yaml::from_slice::<serde_yaml::Value>(&yaml[..end]) else {
            continue;
        };
        let Some(name) = config.get("name").and_then(serde_yaml::Value::as_str) else {
            continue;
        };
        if identity.as_ref().is_some_and(|current| current != name) {
            bail!("Launcher contains conflicting embedded application identities");
        }
        identity = Some(name.to_string());
    }
    identity.context("Launcher has no embedded application identity; rebuild it before creating a frozen distribution")
}

pub(crate) fn validate_file(launcher: &Path, expected: &str) -> Result<()> {
    validate_bytes(&std::fs::read(launcher)?, expected)
}

pub(crate) fn validate_bytes(bytes: &[u8], expected: &str) -> Result<()> {
    let name = name_from_bytes(bytes)?;
    if name != expected {
        bail!("Launcher embeds application '{name}', but the frozen package declares '{expected}'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn launcher_identity_requires_embedded_configuration() {
        assert!(name_from_bytes(b"example.exe").is_err());
        let bytes = [
            b"binary prefix".as_slice(),
            BEGIN,
            b"name: example\nprofiles: []\n",
            END,
            b"binary suffix",
        ]
        .concat();
        assert_eq!(name_from_bytes(&bytes).unwrap(), "example");
        assert!(validate_bytes(&bytes, "example").is_ok());
        assert!(validate_bytes(&bytes, "other").is_err());
        assert_eq!(
            embedded_name().unwrap(),
            serde_yaml::from_str::<serde_yaml::Value>(include_str!("../../assets/pyappify.yml"))
                .unwrap()["name"]
                .as_str()
                .unwrap()
        );
    }
}
