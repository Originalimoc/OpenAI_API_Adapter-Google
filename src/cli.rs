use clap::Parser;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    #[arg(long, value_name = "port", default_value = "18788")]
    pub port: u16,
    #[arg(long, value_name = "upstream_url", default_value = "https://generativelanguage.googleapis.com/v1beta")]
    pub upstream_url: String,
}
