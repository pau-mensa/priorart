//! Test-only failure injection; production builds have no runtime failpoint controls.
#[cfg(test)]
thread_local! { static POINT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) }; }

#[cfg(test)]
pub(crate) fn set(point: Option<&'static str>) {
    POINT.set(point);
}

pub(crate) fn check(point: &str) -> std::io::Result<()> {
    #[cfg(test)]
    if POINT.get() == Some(point) {
        return Err(std::io::Error::other("injected mutation interruption"));
    }
    let _ = point;
    Ok(())
}
