use clap::ValueEnum;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum OutputMode {
    /// Pretty print into console
    #[default]
    Pretty,
    /// Dump results in JSON format
    Json,
    /// Dump results in CSV format
    Csv,
}
