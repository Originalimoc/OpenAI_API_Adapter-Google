use clap::Parser;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    #[arg(long, value_name = "port", default_value = "18788")]
    pub port: u16,
    #[arg(
        long,
        value_name = "upstream_url",
        default_value = "https://generativelanguage.googleapis.com/v1beta"
    )]
    pub upstream_url: String,
    #[arg(long, value_name = "markdown_thought")]
    pub markdown_thought: bool,
    #[arg(long, value_name = "cache_id")]
    pub cache_id: Option<String>,
    /// Enable returning thought summaries (`includeThoughts: true`) by default.
    /// Pass `--include-thoughts=false` to disable.
    #[arg(
        long,
        value_name = "bool",
        default_value_t = true,
        action = clap::ArgAction::Set
    )]
    pub include_thoughts: bool,
    /// Default reasoning effort when the incoming OpenAI request omits `reasoning_effort`.
    /// Values: high (default), medium, low, minimal, none, dynamic
    #[arg(long, value_name = "effort", default_value = "high")]
    pub default_reasoning_effort: String,
}