use relay::lifecycle::{Admission, OperationKind};

fn main() {
    let Admission::Unmanaged(guard) =
        relay::lifecycle::admit_operation("11111111-1111-4111-8111-111111111111", OperationKind::CliInboxDrain)
            .unwrap()
    else {
        return;
    };
    let _ = relay::lifecycle::drain_prior_operations(guard);
}
