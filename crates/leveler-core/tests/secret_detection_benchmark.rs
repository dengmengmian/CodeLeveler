//! Versioned detection benchmark.
//!
//! The corpus is split into positives, ambiguous cases and ordinary
//! development material. The assertions are gates, not a vanity number:
//! missing a known provider key is a security failure, and flagging ordinary
//! code is a product failure. Values are synthetic.

use std::time::Instant;

use leveler_core::{SecretLevel, detect_secrets};

struct Positive {
    text: &'static str,
    /// The exact value spans expected, in order.
    expected: &'static [&'static str],
}

struct Negative {
    text: &'static str,
}

const POSITIVES: &[Positive] = &[
    Positive {
        text: "DEEPSEEK_API_KEY=sk-2f8a1c9e4b7d6a5f3e2c1b0a",
        expected: &["sk-2f8a1c9e4b7d6a5f3e2c1b0a"],
    },
    Positive {
        text: "OPENAI=sk-proj-AbCdEf0123456789xyzXYZ",
        expected: &["sk-proj-AbCdEf0123456789xyzXYZ"],
    },
    Positive {
        text: "GITHUB_TOKEN=ghp_16C7e42F292c6912E7710c838347Ae178B4a",
        expected: &["ghp_16C7e42F292c6912E7710c838347Ae178B4a"],
    },
    Positive {
        text: "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE",
        expected: &["AKIAIOSFODNN7EXAMPLE"],
    },
    Positive {
        text: "SLACK=xoxb-123456789012-1234567890123-AbCdEfGhIjKlMnOpQrStUvWx",
        expected: &["xoxb-123456789012-1234567890123-AbCdEfGhIjKlMnOpQrStUvWx"],
    },
    Positive {
        text: "STRIPE=sk_live_51H8xQ2eZvKYlo2CxyzABCDEF",
        expected: &["sk_live_51H8xQ2eZvKYlo2CxyzABCDEF"],
    },
    Positive {
        text: "GOOGLE=AIzaSyA1234567890abcdefghijklmnopqrstu",
        expected: &["AIzaSyA1234567890abcdefghijklmnopqrstu"],
    },
    Positive {
        text: "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
        expected: &[
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
        ],
    },
    Positive {
        text: "DATABASE_URL=postgres://alice:hunter2-abc123@localhost:5432/app",
        expected: &["hunter2-abc123"],
    },
    Positive {
        text: r#"{"password": "hunter2-abc123"}"#,
        expected: &["hunter2-abc123"],
    },
    Positive {
        text: "client_secret: shhh-very-secret-42",
        expected: &["shhh-very-secret-42"],
    },
    Positive {
        text: "curl -H 'X-Api-Key: Zx81mQpL0aB7' https://x",
        expected: &["Zx81mQpL0aB7"],
    },
    Positive {
        text: "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA1234567890abcdef\n-----END RSA PRIVATE KEY-----",
        expected: &[
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA1234567890abcdef\n-----END RSA PRIVATE KEY-----",
        ],
    },
    Positive {
        text: "db_password: |\n  hunter2-abc123\n  second-line-value\nnext: keep\n",
        expected: &["hunter2-abc123\n  second-line-value"],
    },
    Positive {
        text: "帮我把这个配置进去：API_TOKEN=Zx81mQpL0aB7，谢谢",
        expected: &["Zx81mQpL0aB7"],
    },
];

const AMBIGUOUS: &[&str] = &[
    "API_KEY=abc123def",
    "password=123456",
    "token=example123",
    "DB_PASSWORD=qwerty123",
    "帮我把这个值配置进去：xY9pQ2vT7",
];

const NEGATIVES: &[Negative] = &[
    Negative {
        text: "const apiKey = process.env.API_KEY;",
    },
    Negative {
        text: "let password = config.password;",
    },
    Negative {
        text: r#"api_key = os.getenv("API_KEY")"#,
    },
    Negative {
        text: "pub password: String,",
    },
    Negative {
        text: "client := Config{ Password: redisPassword }",
    },
    Negative {
        text: "token: Option<String>",
    },
    Negative {
        text: "- `password: String`",
    },
    Negative {
        text: "// TODO: validate api_key length",
    },
    Negative {
        text: "password == \"demo-password\"",
    },
    Negative {
        text: "API_KEY=${API_KEY}",
    },
    Negative {
        text: "API_KEY=$API_KEY",
    },
    Negative {
        text: "API_KEY=<YOUR_API_KEY>",
    },
    Negative {
        text: "API_KEY=your-api-key-here",
    },
    Negative {
        text: "API_KEY=changeme",
    },
    Negative {
        text: "DB_PASSWORD=%DB_PASSWORD%",
    },
    Negative {
        text: "password: [REDACTED]",
    },
    Negative { text: "TOKEN=" },
    Negative {
        text: "password: required",
    },
    Negative {
        text: "token: invalid",
    },
    Negative {
        text: r#"{"password": "Enter your password"}"#,
    },
    Negative {
        text: "550e8400-e29b-41d4-a716-446655440000",
    },
    Negative {
        text: "9fceb02d0ae598e95dc970b74767f19372d61af8",
    },
    Negative {
        text: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    },
    Negative {
        text: "SGVsbG8sIHRoaXMgaXMgYSBwbGFpbiB0ZXh0IG1lc3NhZ2U=",
    },
    Negative {
        text: "/Users/mengmian/Develop/app/dengmengmian/CodeLeveler/src/secret.rs",
    },
    Negative {
        text: "https://example.com/docs?page=2",
    },
    Negative {
        text: "see README.md#api_key for details",
    },
    Negative {
        text: "commit 9fceb02d0ae598e95dc970b74767f19372d61af8",
    },
    Negative {
        text: "请检查 password 字段是否为空",
    },
];

fn matched(expected: &str, text: &str) -> bool {
    detect_secrets(text)
        .iter()
        .any(|finding| finding.value(text) == expected)
}

#[test]
fn the_detection_benchmark_meets_its_gates() {
    let mut true_positive = 0usize;
    let mut false_negative = 0usize;
    for positive in POSITIVES {
        for expected in positive.expected {
            if matched(expected, positive.text) {
                true_positive += 1;
            } else {
                false_negative += 1;
                eprintln!("MISSED: {expected:?} in {:?}", positive.text);
            }
        }
    }
    let positive_total = true_positive + false_negative;
    let recall = true_positive as f64 / positive_total as f64;

    let mut ambiguous_hit = 0usize;
    for text in AMBIGUOUS {
        if detect_secrets(text)
            .iter()
            .any(|finding| finding.level == SecretLevel::Ambiguous)
        {
            ambiguous_hit += 1;
        } else {
            eprintln!("MISSED ambiguous: {text:?}");
        }
    }
    let ambiguous_rate = ambiguous_hit as f64 / AMBIGUOUS.len() as f64;

    let mut ordinary_flagged = 0usize;
    for negative in NEGATIVES {
        if !detect_secrets(negative.text).is_empty() {
            ordinary_flagged += 1;
            eprintln!("FALSE POSITIVE: {:?}", negative.text);
        }
    }
    let false_positive_rate = ordinary_flagged as f64 / NEGATIVES.len() as f64;

    // Precision on the positive corpus: a finding counts as imprecise when its
    // span is not one of the expected values.
    let mut precise = 0usize;
    let mut imprecise = 0usize;
    for positive in POSITIVES {
        for finding in detect_secrets(positive.text) {
            if positive
                .expected
                .iter()
                .any(|expected| *expected == finding.value(positive.text))
            {
                precise += 1;
            } else {
                imprecise += 1;
                eprintln!(
                    "IMPRECISE: {:?} in {:?}",
                    finding.value(positive.text),
                    positive.text
                );
            }
        }
    }
    let precision = precise as f64 / (precise + imprecise).max(1) as f64;

    // A session-sized paste must not stall the submit path. Debug builds are
    // slow; the production release is an order of magnitude faster.
    let large = "line with ordinary code and comments\n".repeat(20_000);
    let started = Instant::now();
    let _ = detect_secrets(&large);
    let elapsed = started.elapsed();

    println!(
        "detection: recall={recall:.3} precision={precision:.3} \
         ambiguous={ambiguous_rate:.3} ordinary_fp={false_positive_rate:.3} \
         large={}KB in {elapsed:?}",
        large.len() / 1024
    );

    assert!(recall >= 0.95, "high-confidence recall {recall:.3}");
    assert!(
        precision >= 0.95,
        "high-confidence precision {precision:.3}"
    );
    assert!(
        false_positive_rate == 0.0,
        "ordinary development material was flagged {false_positive_rate:.3}"
    );
    assert!(
        ambiguous_rate >= 0.80,
        "ambiguous corpus coverage {ambiguous_rate:.3}"
    );
    assert!(
        elapsed < std::time::Duration::from_millis(1_500),
        "1 MB took {elapsed:?}"
    );
}
