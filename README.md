# Epistel

Epistel is a small mail server that lets ordinary mail clients (Outlook,
Thunderbird, Apple Mail, mobile clients) read and send mail for addresses
that live on a cloud mail service. The first supported service is Amazon SES.

SES can receive mail and send mail, but it has no mailboxes and speaks no
protocol a mail client can read from. Epistel fills that gap: it keeps the
mailboxes, serves them over IMAP, and accepts outgoing mail over SMTP
Submission, handing it to SES for delivery.

Status: early development. The domain types, the backend traits, their in-memory
implementations and the conformance suites exist; the protocol servers and the
cloud backends do not yet. See:

- [doc/architecture.md](doc/architecture.md) - the design
- [doc/roadmap.md](doc/roadmap.md) - what the first release contains, and
  what is deliberately left for later

The matching infrastructure code lives in the `terraform-aws-epistel`
repository.

## What it is

- IMAP on port 993 and SMTP Submission on port 465, both implicit TLS.
  Nothing else: no STARTTLS, no POP3, no plaintext, no port 25.
- Not a mail transfer agent. Receiving from and delivering to the internet,
  DKIM signing, and spam and virus scanning stay with the mail service.
- Stateless process. Messages live in an object store, everything else in a
  metadata store. Both sit behind traits and are chosen by configuration.

## Building

```
cargo test --workspace --all-features
cargo run -p epistel -- --config epistel.example.toml check
```

## License

Apache-2.0. See [LICENSE](LICENSE).
