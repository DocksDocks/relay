use relay::lifecycle::{Admission, OperationKind};

fn main() {
    let Admission::Unmanaged(mut guard) =
        relay::lifecycle::admit_operation("11111111-1111-4111-8111-111111111111", OperationKind::CliInboxDrain)
            .unwrap()
    else {
        return;
    };
    let _ = relay::store::drain_with_guard(
        &mut guard,
        "22222222-2222-4222-8222-222222222222",
    );
}
