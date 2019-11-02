use futures_util::future::{BoxFuture, FutureExt};
use mdbook::errors::ErrorKind;
use mdbook::errors::Result;
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::{thread, time};
use tokio::net::process::{Child, Command};
//use tokio::io::async_write_ext::AsyncWriteExt;
use tokio::io::AsyncWriteExt;

static MAX_SPAWN_RETRIES: u64 = 5;

pub trait GraphvizRenderer {
    fn render_graphviz<'a>(
        &self,
        code: &'a String,
        output_path: &'a PathBuf,
    ) -> BoxFuture<'a, Result<()>>;
}

pub struct CommandLineGraphviz;

impl GraphvizRenderer for CommandLineGraphviz {
    fn render_graphviz<'a>(
        &self,
        code: &'a String,
        output_path: &'a PathBuf,
    ) -> BoxFuture<'a, Result<()>> {
        async move {
            let output_path_str = output_path.to_str().ok_or_else(|| {
                ErrorKind::Io(io::Error::new(
                    io::ErrorKind::NotFound,
                    "Couldn't build output path",
                ))
            })?;

            let mut child = CommandLineGraphviz::spawn_backoff(output_path_str)?;

            if let Some(mut stdin) = child.stdin().take() {
                stdin.write_all(code.as_bytes()).await?;
            }

            if child.await?.success() {
                Ok(())
            } else {
                Err(ErrorKind::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Error response from Graphviz",
                ))
                .into())
            }
        }
        .boxed()
    }
}

impl CommandLineGraphviz {
    // TODO this doesn't really work that well,
    fn spawn_backoff(output_path_str: &str) -> io::Result<Child> {
        for backoff in 1..=MAX_SPAWN_RETRIES {
            match Command::new("dot")
                .args(&["-Tsvg", "-o", output_path_str])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
            {
                success @ Ok(_) => return success,
                Err(e) => {
                    let sleep_time = backoff.pow(2) * 10;
                    eprintln!(
                        "Failed to spawn process, retrying in {}ms : {}",
                        sleep_time, e
                    );

                    thread::sleep(time::Duration::from_millis(sleep_time))
                }
            }
        }

        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "Couldn't spawn child process",
        ))
    }
}
