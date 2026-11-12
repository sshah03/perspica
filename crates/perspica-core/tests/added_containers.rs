use perspica_core::cross_file::FileChange;
use perspica_core::flow::StepKind;
use perspica_core::manifest::SymbolKind;
use perspica_core::{analyze_multi, Language};

#[test]
fn added_class_methods_appear_in_call_order() {
    let result = analyze_multi(&[
        FileChange::new(
            "main.py",
            "def run():\n    return 1\n",
            "from config import Config\nfrom server import serve\n\ndef run():\n    config = Config.load()\n    return serve(config)\n",
            Language::Python,
        ),
        FileChange::new(
            "config.py",
            "",
            "class Config:\n    @staticmethod\n    def load():\n        return Config.parse()\n\n    @staticmethod\n    def parse():\n        return 1\n",
            Language::Python,
        ),
        FileChange::new(
            "server.py",
            "",
            "def serve(config):\n    return config\n",
            Language::Python,
        ),
    ])
    .unwrap();

    let steps = &result.cross_file.reading_order;
    let functions: Vec<_> = steps
        .iter()
        .filter(|step| step.kind == StepKind::Function && !step.repeat)
        .map(|step| (step.name.as_str(), step.depth))
        .collect();
    assert_eq!(
        functions,
        vec![
            ("run", 0),
            ("Config.load", 1),
            ("Config.parse", 2),
            ("serve", 1)
        ]
    );
    let run = steps.iter().find(|step| step.name == "run").unwrap();
    assert_eq!(run.calls, vec!["Config.load", "serve"]);

    let manifest = &result.file_results[1].1.manifest;
    let class = manifest
        .logic_changes
        .iter()
        .find(|entry| entry.name == "Config")
        .unwrap();
    assert_eq!(class.kind, SymbolKind::Class);
    for (name, line) in [("Config.load", 2), ("Config.parse", 6)] {
        let entries: Vec<_> = manifest
            .logic_changes
            .iter()
            .filter(|entry| entry.name == name)
            .collect();
        assert_eq!(entries.len(), 1, "{manifest:?}");
        let entry = entries[0];
        assert_eq!(entry.kind, SymbolKind::Method);
        assert_eq!(entry.description, "added");
        assert_eq!(entry.location.line_start, line);
        let step = steps.iter().find(|step| step.name == name).unwrap();
        assert_eq!(step.entry_ids, vec![entry.id]);
        assert_ne!(entry.id, class.id);
    }
}

#[test]
fn added_typescript_class_methods_have_their_own_test_reach() {
    let source = "export class Config {\n    static load() {\n        return 1;\n    }\n}\n";
    let result = analyze_multi(&[
        FileChange::new("config.ts", "", source, Language::TypeScript),
        FileChange::new(
            "config.test.ts",
            "",
            "import { Config } from './config';\ntest('loads config', () => { Config.load(); });\n",
            Language::TypeScript,
        ),
    ])
    .unwrap();

    let manifest = &result.file_results[0].1.manifest;
    let method = manifest
        .logic_changes
        .iter()
        .find(|entry| entry.name == "Config.load")
        .unwrap();
    assert_eq!(method.kind, SymbolKind::Method);
    let reach = result
        .cross_file
        .test_reach
        .iter()
        .find(|reach| reach.name == "Config.load")
        .unwrap();
    assert_eq!(reach.entry_id, method.id);
    assert!(!reach.tests.is_empty(), "{reach:?}");
    assert_eq!(reach.via.last().map(String::as_str), Some("Config.load"));
}

#[test]
fn changing_only_an_existing_class_field_does_not_mark_its_methods_changed() {
    let old = "export class Config {\n    value = 1;\n    static load() {\n        return 1;\n    }\n}\nexport function run() { return Config.load(); }\n";
    let new = old
        .replace("value = 1", "value = 2")
        .replace("return Config.load()", "return 1 + Config.load()");
    let result =
        analyze_multi(&[FileChange::new("config.ts", old, new, Language::TypeScript)]).unwrap();
    let steps = &result.cross_file.reading_order;
    assert!(steps.iter().any(|step| step.name == "Config"), "{steps:?}");
    assert!(steps.iter().any(|step| step.name == "run"), "{steps:?}");
    assert!(
        !steps.iter().any(|step| step.name == "Config.load"),
        "{steps:?}"
    );
    assert!(!result.file_results[0]
        .1
        .manifest
        .logic_changes
        .iter()
        .any(|entry| entry.name == "Config.load"));
}
