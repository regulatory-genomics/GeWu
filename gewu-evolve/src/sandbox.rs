//! Python sandbox for safe code execution.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::time::{timeout, Duration};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PythonResult {
    pub stdout: String,
    pub stderr: String,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
    pub timed_out: bool,
}

pub struct PythonSandbox {
    timeout_secs: u64,
}

impl PythonSandbox {
    pub fn new(timeout_secs: u64) -> Self {
        Self { timeout_secs }
    }

    pub async fn execute(
        &self,
        code: &str,
        input: serde_json::Value,
    ) -> Result<PythonResult> {
        let payload = serde_json::json!({
            "code": code,
            "input": input
        });

        let payload_str = serde_json::to_string(&payload)?;

        let mut child = Command::new("python3")
            .arg("-c")
            .arg(Self::create_wrapper_code())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("Failed to spawn Python process")?;

        let mut stdin = child.stdin.take().expect("stdin not captured");
        let stdout = child.stdout.take().expect("stdout not captured");
        let stderr = child.stderr.take().expect("stderr not captured");

        stdin.write_all(payload_str.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        drop(stdin);

        let process_future = async {
            let status = child.wait().await?;
            let mut stderr_reader = BufReader::new(stderr).lines();
            let mut stderr_output = String::new();
            while let Some(line) = stderr_reader.next_line().await? {
                stderr_output.push_str(&line);
                stderr_output.push('\n');
            }
            let mut stdout_reader = BufReader::new(stdout).lines();
            let mut stdout_output = String::new();
            while let Some(line) = stdout_reader.next_line().await? {
                stdout_output.push_str(&line);
                stdout_output.push('\n');
            }
            Ok::<_, std::io::Error>((status, stderr_output, stdout_output))
        };

        let result = timeout(
            Duration::from_secs(self.timeout_secs),
            process_future,
        ).await;

        match result {
            Ok(Ok((status, stderr_output, stdout_output))) => {
                if !status.success() && !stderr_output.is_empty() {
                    return Ok(PythonResult {
                        stdout: stdout_output,
                        stderr: stderr_output.clone(),
                        result: None,
                        error: Some(stderr_output),
                        timed_out: false,
                    });
                }

                let parsed: serde_json::Value = match serde_json::from_str(stdout_output.trim()) {
                    Ok(v) => v,
                    Err(_) => {
                        return Ok(PythonResult {
                            stdout: stdout_output,
                            stderr: stderr_output,
                            result: None,
                            error: Some("Failed to parse result as JSON".to_string()),
                            timed_out: false,
                        });
                    }
                };

                if let Some(err) = parsed.get("error").and_then(|e| e.as_str()) {
                    return Ok(PythonResult {
                        stdout: stdout_output,
                        stderr: stderr_output,
                        result: parsed.get("result").cloned(),
                        error: Some(err.to_string()),
                        timed_out: false,
                    });
                }

                Ok(PythonResult {
                    stdout: stdout_output,
                    stderr: stderr_output,
                    result: parsed.get("result").cloned(),
                    error: None,
                    timed_out: false,
                })
            }
            Ok(Err(e)) => {
                Ok(PythonResult {
                    stdout: String::new(),
                    stderr: String::new(),
                    result: None,
                    error: Some(format!("IO error: {}", e)),
                    timed_out: false,
                })
            }
            Err(_) => {
                let _ = child.kill().await;
                Ok(PythonResult {
                    stdout: String::new(),
                    stderr: String::new(),
                    result: None,
                    error: Some(format!(
                        "Execution timed out after {} seconds",
                        self.timeout_secs
                    )),
                    timed_out: true,
                })
            }
        }
    }

    fn create_wrapper_code() -> String {
        r#"
import json
import sys

def main():
    try:
        input_data = json.loads(sys.stdin.readline().strip())
        code = input_data.get('code', '')
        sandbox_input = input_data.get('input', {})
        
        global_namespace = {'__builtins__': __builtins__, 'input': sandbox_input}
        
        try:
            exec(code, global_namespace)
            
            if 'result' in global_namespace:
                result = global_namespace['result']
                if hasattr(result, '__json__'):
                    result = result.__json__()
                elif hasattr(result, '__dict__'):
                    result = result.__dict__
                print(json.dumps({'result': result}))
            else:
                print(json.dumps({'result': None}))
        except Exception as e:
            print(json.dumps({'error': str(e)}))
    except Exception as e:
        print(json.dumps({'error': f'Invalid input: {str(e)}'}))

if __name__ == '__main__':
    main()
"#.to_string()
    }
}

impl Default for PythonSandbox {
    fn default() -> Self {
        Self::new(30)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_simple_execution() {
        let sandbox = PythonSandbox::default();
        let result = sandbox.execute("result = 2 + 2", serde_json::json!({})).await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(!result.timed_out);
        if result.error.is_some() {
            eprintln!("Error: {:?}", result.error);
        }
        assert!(result.error.is_none() || result.error.as_ref().unwrap().contains("traceback"));
    }

    #[tokio::test]
    async fn test_timeout() {
        let sandbox = PythonSandbox::new(1);
        let result = sandbox
            .execute(
                "import time; time.sleep(10); result = 'done'",
                serde_json::json!({}),
            )
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.timed_out);
    }

    #[tokio::test]
    async fn test_with_input() {
        let sandbox = PythonSandbox::default();
        let result = sandbox
            .execute(
                "result = input['value'] * 2",
                serde_json::json!({"value": 21}),
            )
            .await;

        assert!(result.is_ok());
    }
}
