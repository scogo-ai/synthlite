mod common;

use serde_json::json;
use sha2::{Digest, Sha256};

use synthlite::identity::{
    generator_config_hash, identity_value, norm_hash, normalize_prompt, population_sha256,
    prompt_task_id, source_population_sha256, source_task_id,
};

// Golden vectors from Taskgen's canonical fixture task and its Python golden
// values, so a Taskgen seed keeps the id Taskgen gave it.
// `language`, `taskgen_model`, and `temperature` stay out of the task id.

#[test]
fn source_task_id_matches_taskgen_golden_vectors() {
    let first = common::fixture_task();
    let mut second = first.clone();
    second["prompt"] = json!("Unicode café 路由 incident");

    assert_eq!(
        source_task_id(&first).unwrap(),
        "task_cc3e0bec7b87ec223ae0ef01f4d4235ff26f8b5111d6590c39c8c7db13a88a7f"
    );
    assert_eq!(
        source_task_id(&second).unwrap(),
        "task_8a45fd82c66ff7d94244f25acc28ec84d57d96bb85ebf50f777a3c74bf1ddf34"
    );
    assert_eq!(
        source_population_sha256(&[first, second]).unwrap(),
        "f01f0b22eac765cec916eb7f1b92973ddf53b3472705967445a53c87f0392a76"
    );
}

#[test]
fn markup_prompt_is_not_html_escaped() {
    let mut task = common::fixture_task();
    task["prompt"] = json!("probe & compare <edge> > baseline");
    let identity = identity_value(&task);
    let bytes = serde_json::to_vec(&identity).unwrap();
    let text = String::from_utf8(bytes.clone()).unwrap();
    assert!(text.contains("probe & compare <edge> > baseline"), "{text}");
    assert!(!text.contains("\\u003c"), "{text}");
    assert!(!text.contains("&lt;"), "{text}");
    let digest = format!("{:x}", Sha256::digest(&bytes));
    assert_eq!(source_task_id(&task).unwrap(), format!("task_{digest}"));
}

#[test]
fn language_model_and_temperature_do_not_change_task_id() {
    let mut changed = common::fixture_task();
    changed["language"] = json!("en");
    changed["taskgen_model"] = json!("other-teacher");
    changed["temperature"] = json!(0.1);
    assert_eq!(
        source_task_id(&common::fixture_task()).unwrap(),
        source_task_id(&changed).unwrap()
    );
    assert_ne!(
        source_population_sha256(&[common::fixture_task()]).unwrap(),
        source_population_sha256(&[changed]).unwrap()
    );
}

#[test]
fn curriculum_is_part_of_task_identity() {
    let task = common::fixture_task();
    let mut revised = task.clone();
    revised["coordinates"]["curriculum"] = json!({
        "context": {"audience": "l0_self_service"},
        "objective": "Check encrypted DNS routing",
        "acceptance": ["Identify the actual resolver"],
        "references": ["https://www.rfc-editor.org/rfc/rfc8484"]
    });
    assert_ne!(
        source_task_id(&task).unwrap(),
        source_task_id(&revised).unwrap()
    );
}

#[test]
fn generator_hash_distinguishes_one_from_one_point_zero() {
    let base = json!({
        "schema_version": "synthlite.generator-config.v1",
        "system_message": null,
        "temperature": null,
        "top_p": null,
        "max_output_tokens": null,
        "seed": null,
        "frequency_penalty": null,
        "presence_penalty": null,
        "stop": null,
        "reasoning_effort": null
    });
    let mut integer = base.clone();
    integer["temperature"] = json!(1);
    let mut float = base.clone();
    float["temperature"] = json!(1.0);
    let integer_hash = generator_config_hash(&integer).unwrap();
    let float_hash = generator_config_hash(&float).unwrap();
    assert_ne!(integer_hash, float_hash);
    assert!(integer_hash.starts_with("gen_"));
    assert!(serde_json::to_string(&float).unwrap().contains("1.0"));
    assert!(!serde_json::to_string(&integer).unwrap().contains("1.0"));
    assert_eq!(
        generator_config_hash(&base).unwrap(),
        generator_config_hash(&base).unwrap()
    );
}

#[test]
fn normalize_prompt_matches_taskgen() {
    assert_eq!(normalize_prompt("  Foo\n\tBAR  baz "), "foo bar baz");
    assert_eq!(norm_hash("  Foo   bar"), norm_hash("foo bar"));
}

// The bytes hashed are `{"prompt":<prompt>,"schema_version":"synthlite.prompt.v1"}`,
// so any tool can recompute a prompt's id with sha256.
#[test]
fn prompt_task_id_matches_golden_vectors() {
    assert_eq!(
        prompt_task_id("Why are BGP paths stale?").unwrap(),
        "task_0afa56cf133efb955c286893d72e49522dffe6cccdd4b13e39af4c87304363fa"
    );
    assert_eq!(
        prompt_task_id("Unicode café 路由 incident").unwrap(),
        "task_fe9cde9ee39de9704c5ec1e57740fca2c2d431ce40aca80ae02db8d946031d35"
    );
    let bytes = br#"{"prompt":"Why are BGP paths stale?","schema_version":"synthlite.prompt.v1"}"#;
    assert_eq!(
        prompt_task_id("Why are BGP paths stale?").unwrap(),
        format!("task_{:x}", Sha256::digest(bytes))
    );
    // A prompt never shares an id with a Taskgen task of the same text.
    let mut task = common::fixture_task();
    task["prompt"] = json!("Why are BGP paths stale?");
    assert_ne!(
        source_task_id(&task).unwrap(),
        prompt_task_id("Why are BGP paths stale?").unwrap()
    );
}

#[test]
fn population_digest_of_taskgen_tasks_is_unchanged() {
    let first = common::fixture_task();
    let mut second = first.clone();
    second["prompt"] = json!("Unicode café 路由 incident");
    // Any order of (id, task) pairs gives the Taskgen golden digest.
    let entries = vec![
        (source_task_id(&second).unwrap(), &second),
        (source_task_id(&first).unwrap(), &first),
    ];
    assert_eq!(
        population_sha256(&entries).unwrap(),
        "f01f0b22eac765cec916eb7f1b92973ddf53b3472705967445a53c87f0392a76"
    );
}

#[test]
fn usage_is_the_eighteen_line_contract() {
    let lines: Vec<_> = synthlite::USAGE.lines().collect();
    assert_eq!(lines.len(), 18, "{}", synthlite::USAGE);
    assert!(
        synthlite::USAGE.starts_with("synthlite — prompts in, a private fine-tuning dataset out\n")
    );
    assert!(synthlite::USAGE.contains("synthlite generate --help for every flag"));
    assert!(!synthlite::USAGE.contains("Taskgen"));
    assert!(synthlite::USAGE.contains("synthlite --version"));
    assert!(
        synthlite::USAGE.contains("set OPENAI_MODEL or SYNTHLITE_MODEL")
            || synthlite::USAGE.contains("OPENAI_MODEL or SYNTHLITE_MODEL")
    );
    assert!(synthlite::USAGE.contains("Switching --detailed refuses."));
}
