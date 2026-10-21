use std::fmt::Write as _;

use parrot_domain::contracts::paths::{
    PrimaryReadFailure, ReadPurpose, RootOrigin, SelectedSessionRoot, allows_legacy_fallback,
};
use parrot_domain::values::config::{PathPlatform, PlatformPath};
use serde_json::Value;

use crate::support::for_cases;

#[test]
fn seeded_lexical_paths_keep_selected_root_and_approved_fallback_is_exact() {
    let corpus: Value = serde_json::from_str(include_str!(
        "../fixtures/regressions/T77/path-regressions.json"
    ))
    .unwrap();
    for row in corpus["lexical"].as_array().unwrap() {
        let platform = match row["platform"].as_str().unwrap() {
            "Unix" => PathPlatform::Unix,
            "MacOs" => PathPlatform::MacOs,
            other => panic!("unknown committed path platform {other}"),
        };
        let observed = PlatformPath::normalize(
            row["input"].as_str().unwrap(),
            row["base"].as_str().unwrap(),
            platform,
        );
        if let Some(error) = row.get("error") {
            assert_eq!(
                format!("{:?}", observed.unwrap_err()),
                error.as_str().unwrap(),
                "{row}"
            );
        } else {
            assert_eq!(
                observed.unwrap(),
                row["expected"].as_str().unwrap(),
                "{row}"
            );
        }
    }
    let purposes = [
        ReadPurpose::ApplicationState,
        ReadPurpose::ApplicationData,
        ReadPurpose::SessionInternal,
        ReadPurpose::Configuration,
        ReadPurpose::Credentials,
        ReadPurpose::UserFile,
    ];
    let failures = [
        PrimaryReadFailure::Absent,
        PrimaryReadFailure::PermissionDenied,
        PrimaryReadFailure::Corrupt,
        PrimaryReadFailure::Other,
    ];
    let table = &corpus["fallback_table"];
    let mut rows = 0;
    for (enabled_index, enabled) in [false, true].into_iter().enumerate() {
        for (purpose_index, purpose) in purposes.into_iter().enumerate() {
            assert_eq!(
                format!("{purpose:?}"),
                table["purposes"][purpose_index].as_str().unwrap()
            );
            for (failure_index, failure) in failures.into_iter().enumerate() {
                assert_eq!(
                    format!("{failure:?}"),
                    table["failures"][failure_index].as_str().unwrap()
                );
                assert_eq!(
                    allows_legacy_fallback(enabled, purpose, failure),
                    table["expected"][enabled_index][purpose_index][failure_index]
                        .as_bool()
                        .unwrap(),
                    "approved fallback enabled={enabled} purpose={purpose:?} failure={failure:?}"
                );
                rows += 1;
            }
        }
    }
    assert_eq!(rows, 48);
    for origin in [RootOrigin::Current, RootOrigin::Legacy] {
        let selected = SelectedSessionRoot::new("/synthetic/session".into(), origin).unwrap();
        for invalid in ["", "../escape", "queues/../escape", "/absolute", "./queues"] {
            assert!(
                selected.resolve_internal(invalid).is_err(),
                "lexical internal path {invalid:?}"
            );
        }
        // Path::components normalizes interior dots/repeated separators; this is not raw-string grammar.
        assert!(selected.resolve_internal("queues//./one.json").is_ok());
    }
    for_cases("path", |case, generator| {
        let count = 1 + generator.pick(8);
        let names: Vec<_> = (0..count)
            .map(|_| format!("node-{}", generator.next()))
            .collect();
        let mut encoded = String::new();
        for name in &names {
            write!(&mut encoded, "./{name}/../{name}//").unwrap();
        }
        let expected = format!("/synthetic/base/{}/", names.join("/"));
        let normalized =
            PlatformPath::normalize(&encoded, "/synthetic/base", PathPlatform::Unix).unwrap();
        assert_eq!(normalized, expected, "{case} input={encoded:?}");
        assert_eq!(
            PlatformPath::normalize(&normalized, "/unused", PathPlatform::Unix).unwrap(),
            normalized,
            "{case}"
        );
        let origin = if generator.pick(2) == 0 {
            RootOrigin::Current
        } else {
            RootOrigin::Legacy
        };
        let selected = SelectedSessionRoot::new("/synthetic/session".into(), origin).unwrap();
        let relative = format!("queues/item-{}.json", generator.next());
        let resolved = selected.resolve_internal(&relative).unwrap();
        assert_eq!(resolved.selected_root(), &selected, "{case}");
        assert_eq!(resolved.path(), selected.path().join(&relative), "{case}");
        assert_eq!(resolved.selected_root().origin(), origin, "{case}");
        assert!(
            selected
                .resolve_internal(format!("{relative}/../escape"))
                .is_err(),
            "{case}"
        );
    });
}
