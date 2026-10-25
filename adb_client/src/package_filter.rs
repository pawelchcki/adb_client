/// Which installed Android packages to list.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PackageFilter {
    /// System and user-installed packages.
    #[default]
    All,
    /// Packages installed on the system image.
    System,
    /// User-installed packages.
    ThirdParty,
}
