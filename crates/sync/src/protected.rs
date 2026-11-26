use archive_domain::*;

/// The active reference file is not a user data-operation target. Internal file
/// transport uses another port; no database format or transfer token lives here.
pub fn check_protected(
    location: &Location,
    protected: Option<&ObjectId>,
) -> Result<(), OperationError> {
    if matches!((location,protected),(Location::Remote(id),Some(active)) if id==active) {
        Err(OperationError::new(
            ErrorKind::Input,
            "active-index",
            format!(
                "active index {:?} cannot be used as a data-operation target",
                location.display()
            ),
        )
        .at(location))
    } else {
        Ok(())
    }
}
