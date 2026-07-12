fn main() {
    let _permit = relay::lifecycle::ChildCancellationPermit {
        supervisor_instance_id: "forged".to_string(),
        operation_id: "forged".to_string(),
        operation_version: "1".to_string(),
        child_slot: 0,
    };
}
