use parrot_domain::values::services::AgentSessionServices;

#[test]
fn returns_exact_instances_rejects_missing_and_duplicate_keys_and_isolates_registries() {
    let mut first = AgentSessionServices::default();
    let mut second = AgentSessionServices::default();
    let first_value = String::from("first");
    let second_value = String::from("second");
    first.register::<String>(first_value.clone()).unwrap();
    second.register::<String>(second_value.clone()).unwrap();

    let retrieved = first.get_service::<String>().unwrap();
    assert_eq!(retrieved, &first_value);
    assert!(std::ptr::eq(
        retrieved,
        first.get_service::<String>().unwrap()
    ));
    assert_eq!(second.get_service::<String>().unwrap(), &second_value);
    assert!(!std::ptr::eq(
        first.get_service::<String>().unwrap(),
        second.get_service::<String>().unwrap()
    ));
    assert!(first.get_service::<u8>().is_err());
    assert!(first.register::<String>("duplicate".to_owned()).is_err());
}
