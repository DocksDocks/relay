fn main() {
    let _proof = relay::lifecycle::OwnedChildReapProof {
        worker_id: "forged".to_string(),
        generation: "forged".to_string(),
        operation_id: "forged".to_string(),
        supervisor_instance_id: "forged".to_string(),
        pid: 1,
        exit_status: 0,
        operation_version: "1".to_string(),
    };
}
