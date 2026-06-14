//! Property test: any AgentConfig survives a YAML serialize -> parse round-trip.

use cowboy_core::config::*;
use proptest::prelude::*;

fn arb_process() -> impl Strategy<Value = ProcessDef> {
    (
        "[a-z][a-z0-9 ._/-]{0,40}",
        "/[a-z0-9/]{0,20}",
        any::<bool>(),
    )
        .prop_map(|(command, cwd, auto_start)| ProcessDef {
            command,
            cwd,
            auto_start,
        })
}

fn arb_agent_config() -> impl Strategy<Value = AgentConfig> {
    (
        1u32..=3,
        0u64..100_000,
        0u64..100_000,
        0u32..10_000,
        0usize..10_000_000,
        "[a-z0-9._/-]{1,40}",
        prop::collection::btree_map("[a-z][a-z0-9_-]{0,12}", arb_process(), 0..4),
        prop::collection::btree_map("[a-z][a-z0-9_-]{0,12}", "[a-z0-9 ._/-]{1,40}", 0..4),
    )
        .prop_map(
            |(
                version,
                command_timeout_seconds,
                model_timeout_seconds,
                max_iterations,
                max_command_output_bytes,
                scratchpad,
                processes,
                commands,
            )| AgentConfig {
                version,
                agent: AgentBehavior {
                    command_timeout_seconds,
                    model_timeout_seconds,
                    max_iterations,
                    max_command_output_bytes,
                },
                session: SessionConfig { scratchpad },
                processes,
                commands,
            },
        )
}

proptest! {
    #[test]
    fn agent_config_roundtrips(cfg in arb_agent_config()) {
        let yaml = serde_yaml_ng::to_string(&cfg).unwrap();
        let parsed: AgentConfig = serde_yaml_ng::from_str(&yaml).unwrap();
        prop_assert_eq!(cfg, parsed);
    }

    #[test]
    fn empty_maps_omitted_and_restored(
        v in 1u32..3,
    ) {
        // A config with empty maps must still round-trip to an equal value.
        let cfg = AgentConfig { version: v, ..Default::default() };
        let yaml = serde_yaml_ng::to_string(&cfg).unwrap();
        let parsed: AgentConfig = serde_yaml_ng::from_str(&yaml).unwrap();
        prop_assert_eq!(cfg, parsed);
    }
}
