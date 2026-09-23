use crate::{Config, configured_models, connectors, package_source::ResolvedPackages};
use serde_json::{Value, json};
use std::{error::Error, fs};

pub fn validate(config: &Config, packages: &ResolvedPackages) -> Result<(), Box<dyn Error>> {
    connectors(config, packages)?;
    if let Some(selector) = &config.model_instance {
        let (id, instance, _) = config.component(selector)?;
        let (package_id, name) = id.split_once('/').unwrap_or((&id, ""));
        let component = packages
            .get(package_id)?
            .component(name)
            .ok_or("missing model component")?;
        if !configured_models(component, &instance.config, true).contains(&config.model) {
            return Err(format!("model {} is not provided by {selector}", config.model).into());
        }
    }
    for (selector, capabilities) in &config.trusted_capabilities {
        let (id, _, _) = config.component(selector)?;
        let (package_id, name) = id.split_once('/').unwrap_or((&id, ""));
        let package = packages.get(package_id)?;
        let component = package
            .component(name)
            .ok_or("missing capability component")?;
        for capability in capabilities {
            let declaration = component
                .manifest()
                .provides
                .iter()
                .find(|provided| &provided.capability == capability)
                .ok_or_else(|| format!("{selector} does not provide {capability}"))?;
            if let Some(schema) = &declaration.constraints_schema {
                let schema: Value =
                    serde_json::from_slice(&fs::read(package.root().join(schema))?)?;
                let constraints = config
                    .trusted_constraints
                    .get(selector)
                    .and_then(|grants| grants.get(capability))
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                jsonschema::validator_for(&schema)?
                    .validate(&constraints)
                    .map_err(|error| {
                        format!("invalid constraints for {selector} {capability}: {error}")
                    })?;
            }
        }
    }
    let mut timers = false;
    let mut executors = 0;
    for id in config.plugin_instances.keys() {
        for component in packages.get(id)?.components().values() {
            let emits = &component.manifest().emits;
            timers |= emits.iter().any(|event| event == "timer.set");
            executors += usize::from(emits.iter().any(|event| event == "timer.fired"));
        }
    }
    if (timers && executors != 1) || executors > 1 {
        return Err("timer producers require exactly one timer.fired provider".into());
    }
    Ok(())
}
