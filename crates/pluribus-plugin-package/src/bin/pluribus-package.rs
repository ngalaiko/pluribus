use pluribus_plugin_package::build_component_package;
use std::{collections::BTreeMap, env, path::PathBuf, process::ExitCode};

fn main() -> ExitCode {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    if arguments.len() < 3 {
        eprintln!("usage: pluribus-package <template-dir> <output-dir> <name=core-module.wasm>...");
        return ExitCode::from(2);
    }
    let mut modules = BTreeMap::new();
    for binding in &arguments[2..] {
        let Some((name, path)) = binding.split_once('=') else {
            eprintln!("invalid module binding");
            return ExitCode::from(2);
        };
        if name.is_empty()
            || path.is_empty()
            || modules.insert(name.into(), PathBuf::from(path)).is_some()
        {
            eprintln!("invalid or duplicate module binding");
            return ExitCode::from(2);
        }
    }
    match build_component_package(&arguments[0], &modules, &arguments[1]) {
        Ok(digests) => {
            for (name, digest) in digests {
                println!("{name}={digest}");
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
