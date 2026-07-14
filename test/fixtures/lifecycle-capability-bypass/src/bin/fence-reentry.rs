fn main() {
    let intent = relay::lifecycle::publish_fence(
        "11111111-1111-4111-8111-111111111111",
        "22222222-2222-4222-8222-222222222222",
        "compile-fail",
    )
    .unwrap();
    let mut permit = relay::lifecycle::drain_prior_operations(intent).unwrap();
    let _ = relay::store::drain_with_guard(&mut permit);
}
