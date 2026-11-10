//! The metadata store on DynamoDB.
//!
//! One table, partition key `pk`, sort key `sk`, both strings. Every read is strongly consistent:
//! a mail client that stores a flag and reads it back must see it.
//!
//! | `pk` | `sk` | What |
//! |---|---|---|
//! | `ACCOUNT#<account>` | `META` | the account, and its counter for `UIDVALIDITY` values |
//! | `ACCOUNT#<account>` | `CRED#<credential>` | one credential |
//! | `ACCOUNT#<account>` | `ADDR#<pattern>` | marks a pattern the account owns, for listing |
//! | `ACCOUNT#<account>` | `MBOXNAME#<name>` | reserves a mailbox name, points at the mailbox |
//! | `ACCOUNTS` | `<primary address>` | the directory of accounts, for listing |
//! | `ADDR#<pattern>` | `META` | the owner of a pattern |
//! | `MBOX#<mailbox>` | `META` | the mailbox with its counters |
//! | `MBOX#<mailbox>` | `MSG#<uid, ten digits>` | one message record |
//! | `BLOB#<blob>` | `META` | how many records point at a blob |
//! | `INGEST#<key>` | `META` | a claimed delivery; expires |
//!
//! `expires_at` (seconds since the epoch) is the table's time-to-live attribute.
//!
//! Where one call has to change several items it uses a transaction. Three things are done in
//! two steps instead, because a transaction over the mailbox's counters would make every writer
//! to a busy mailbox collide with every other:
//!
//! - An append first takes a UID from the mailbox, then stores the record. A failure in between
//!   leaves a UID unused, which IMAP allows.
//! - Removing a record and releasing its blob are separate. A failure in between leaves a blob
//!   that nothing points at and nobody was told to delete.
//! - Deleting a mailbox or an account removes the entry first and its contents after. A failure
//!   in between leaves items nothing leads to.
//!
//! None of these loses mail or shows a client something wrong; they leave garbage for a sweep.

use std::collections::{BTreeSet, HashMap};
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::operation::transact_write_items::TransactWriteItemsError;
use aws_sdk_dynamodb::types::{
    AttributeDefinition, AttributeValue as Av, BillingMode, ConditionCheck, Delete, KeySchemaElement, KeyType,
    KeysAndAttributes, Put, ReturnValue, ScalarAttributeType, TableStatus, TransactWriteItem, Update,
};
use epistel_core::model::{
    Account, AccountState, Credential, Expunged, FlagChange, Flags, Keyword, Mailbox, MailboxName, MailboxRole,
    MessageRecord, ModSeq, NewAccount, NewCredential, NewMessage, SystemFlags, Uid, UidRange, UidValidity,
};
use epistel_core::{
    AccountId, AddressPattern, BlobId, CredentialId, EmailAddress, Error, MailboxId, MetadataStore, Result,
};

use crate::fault;

type Item = HashMap<String, Av>;

const META: &str = "META";
const ACCOUNTS: &str = "ACCOUNTS";
/// How many items one transaction or one batch read may hold.
const BATCH: usize = 100;
/// How often a call is tried again when it collides with another writer.
const ATTEMPTS: u32 = 10;
/// How long a released blob's marker stays, so that a late append of that blob is still refused.
const RELEASED_MARKER_LIFETIME: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// A [`MetadataStore`] in one DynamoDB table.
pub struct DynamoMetadataStore {
    client: Client,
    table: String,
}

// ---------------------------------------------------------------------------------------------
// Keys and attribute values
// ---------------------------------------------------------------------------------------------

fn account_pk(id: AccountId) -> String {
    format!("ACCOUNT#{id}")
}

fn address_pk(pattern: &AddressPattern) -> String {
    format!("ADDR#{pattern}")
}

fn mailbox_pk(id: MailboxId) -> String {
    format!("MBOX#{id}")
}

fn blob_pk(id: BlobId) -> String {
    format!("BLOB#{id}")
}

fn credential_sk(id: CredentialId) -> String {
    format!("CRED#{id}")
}

fn address_sk(pattern: &AddressPattern) -> String {
    format!("ADDR#{pattern}")
}

fn name_sk(name: &MailboxName) -> String {
    format!("MBOXNAME#{name}")
}

fn message_sk(uid: Uid) -> String {
    format!("MSG#{:010}", uid.get())
}

fn s(value: impl Into<String>) -> Av {
    Av::S(value.into())
}

fn n(value: impl ToString) -> Av {
    Av::N(value.to_string())
}

fn time_value(time: SystemTime) -> Av {
    n(time.duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos())
}

fn epoch_seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn key(pk: impl Into<String>, sk: impl Into<String>) -> Item {
    HashMap::from([("pk".to_owned(), s(pk)), ("sk".to_owned(), s(sk))])
}

fn item<const N: usize>(pk: impl Into<String>, sk: impl Into<String>, attributes: [(&str, Av); N]) -> Item {
    let mut item = key(pk, sk);
    item.extend(attributes.into_iter().map(|(name, value)| (name.to_owned(), value)));
    item
}

fn values<const N: usize>(pairs: [(&str, Av); N]) -> Option<HashMap<String, Av>> {
    Some(pairs.into_iter().map(|(name, value)| (name.to_owned(), value)).collect())
}

// ---------------------------------------------------------------------------------------------
// Reading items back
// ---------------------------------------------------------------------------------------------

fn malformed(attribute: &str) -> Error {
    Error::unavailable(format!("stored item has no usable `{attribute}`"))
}

fn text<'a>(item: &'a Item, name: &str) -> Result<&'a str> {
    item.get(name).and_then(|value| value.as_s().ok()).map(String::as_str).ok_or_else(|| malformed(name))
}

fn parsed<T: FromStr>(item: &Item, name: &str) -> Result<T> {
    text(item, name)?.parse().map_err(|_| malformed(name))
}

fn number<T: FromStr>(item: &Item, name: &str) -> Result<T> {
    let digits = item.get(name).and_then(|value| value.as_n().ok()).ok_or_else(|| malformed(name))?;
    digits.parse().map_err(|_| malformed(name))
}

fn flag(item: &Item, name: &str) -> bool {
    item.get(name).and_then(|value| value.as_bool().ok()).copied().unwrap_or(false)
}

fn time(item: &Item, name: &str) -> Result<SystemTime> {
    let nanos: u128 = number(item, name)?;
    let seconds = u64::try_from(nanos / 1_000_000_000).map_err(|_| malformed(name))?;
    Ok(UNIX_EPOCH + Duration::new(seconds, (nanos % 1_000_000_000) as u32))
}

/// The identifier that follows `prefix` in a key attribute.
fn id_after<T: FromStr>(item: &Item, attribute: &str, prefix: &str) -> Result<T> {
    let rest = text(item, attribute)?.strip_prefix(prefix).ok_or_else(|| malformed(attribute))?;
    rest.parse().map_err(|_| malformed(attribute))
}

fn account_from(item: &Item) -> Result<Account> {
    Ok(Account {
        id: id_after(item, "pk", "ACCOUNT#")?,
        primary_address: parsed(item, "primary_address")?,
        display_name: text(item, "display_name")?.to_owned(),
        state: match text(item, "account_state")? {
            "active" => AccountState::Active,
            "suspended" => AccountState::Suspended,
            _ => return Err(malformed("account_state")),
        },
        created: time(item, "created_at")?,
    })
}

fn state_value(state: AccountState) -> Av {
    s(match state {
        AccountState::Active => "active",
        AccountState::Suspended => "suspended",
    })
}

fn credential_from(item: &Item) -> Result<Credential> {
    Ok(Credential {
        id: id_after(item, "sk", "CRED#")?,
        label: text(item, "cred_label")?.to_owned(),
        secret_hash: text(item, "secret_hash")?.to_owned(),
        created: time(item, "created_at")?,
        last_used: if item.contains_key("last_used") { Some(time(item, "last_used")?) } else { None },
    })
}

const ROLES: [(MailboxRole, &str); 7] = [
    (MailboxRole::Inbox, "inbox"),
    (MailboxRole::Drafts, "drafts"),
    (MailboxRole::Sent, "sent"),
    (MailboxRole::Junk, "junk"),
    (MailboxRole::Trash, "trash"),
    (MailboxRole::Archive, "archive"),
    (MailboxRole::Quarantine, "quarantine"),
];

fn mailbox_from(item: &Item) -> Result<Mailbox> {
    let role = match item.get("mailbox_role") {
        None => None,
        Some(_) => {
            let stored = text(item, "mailbox_role")?;
            let found = ROLES.iter().find(|(_, name)| *name == stored).ok_or_else(|| malformed("mailbox_role"))?;
            Some(found.0)
        }
    };
    let name = MailboxName::parse(text(item, "mailbox_name")?).map_err(|_| malformed("mailbox_name"))?;
    let uid_validity =
        std::num::NonZeroU32::new(number(item, "uid_validity")?).ok_or_else(|| malformed("uid_validity"))?;
    Ok(Mailbox {
        id: id_after(item, "pk", "MBOX#")?,
        account: parsed(item, "account_id")?,
        name,
        role,
        subscribed: flag(item, "is_subscribed"),
        uid_validity: UidValidity(uid_validity),
        next_uid: Uid::new(number(item, "next_uid")?).ok_or_else(|| malformed("next_uid"))?,
        highest_modseq: ModSeq(number(item, "highest_modseq")?),
    })
}

fn message_from(mailbox: MailboxId, item: &Item) -> Result<MessageRecord> {
    let uid: u32 = id_after(item, "sk", "MSG#")?;
    let mut keywords = BTreeSet::new();
    if let Some(stored) = item.get("keyword_set") {
        for keyword in stored.as_ss().map_err(|_| malformed("keyword_set"))? {
            keywords.insert(Keyword::parse(keyword).map_err(|_| malformed("keyword_set"))?);
        }
    }
    Ok(MessageRecord {
        mailbox,
        uid: Uid::new(uid).ok_or_else(|| malformed("sk"))?,
        blob: parsed(item, "blob_id")?,
        size: number(item, "byte_size")?,
        internal_date: time(item, "internal_date")?,
        flags: Flags { system: SystemFlags::from_bits(number(item, "flag_bits")?), keywords },
        modseq: ModSeq(number(item, "mod_seq")?),
    })
}

fn keyword_value(flags: &Flags) -> Option<Av> {
    // DynamoDB has no empty set; a message without keywords has no such attribute.
    (!flags.keywords.is_empty()).then(|| Av::Ss(flags.keywords.iter().map(|k| k.as_str().to_owned()).collect()))
}

// ---------------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------------

/// Whether a failed call failed because its condition did not hold.
macro_rules! condition_failed {
    ($error:expr) => {
        $error.as_service_error().is_some_and(|service| service.is_conditional_check_failed_exception())
    };
}

/// Whether a failed call collided with a transaction on the same item and is worth repeating.
macro_rules! collided {
    ($error:expr) => {
        $error.as_service_error().is_some_and(|service| service.is_transaction_conflict_exception())
    };
}

async fn back_off(attempt: u32) {
    tokio::time::sleep(Duration::from_millis(10 * u64::from(attempt + 1))).await;
}

impl DynamoMetadataStore {
    pub fn new(client: Client, table: impl Into<String>) -> Self {
        Self { client, table: table.into() }
    }

    /// Creates the table with on-demand billing and waits until it can be used.
    ///
    /// For tests and local emulators. A deployment gets its table, with encryption, recovery and
    /// time-to-live on `expires_at`, from the infrastructure module.
    pub async fn create_table(client: &Client, table: &str) -> Result<()> {
        let attribute = |name: &str| {
            AttributeDefinition::builder().attribute_name(name).attribute_type(ScalarAttributeType::S).build()
        };
        let key_part = |name: &str, kind| KeySchemaElement::builder().attribute_name(name).key_type(kind).build();
        client
            .create_table()
            .table_name(table)
            .billing_mode(BillingMode::PayPerRequest)
            .attribute_definitions(attribute("pk").map_err(fault)?)
            .attribute_definitions(attribute("sk").map_err(fault)?)
            .key_schema(key_part("pk", KeyType::Hash).map_err(fault)?)
            .key_schema(key_part("sk", KeyType::Range).map_err(fault)?)
            .send()
            .await
            .map_err(fault)?;
        for attempt in 0..100 {
            let described = client.describe_table().table_name(table).send().await.map_err(fault)?;
            if described.table().and_then(|t| t.table_status()) == Some(&TableStatus::Active) {
                return Ok(());
            }
            back_off(attempt.min(20)).await;
        }
        Err(Error::unavailable("table did not become active"))
    }

    async fn get(&self, pk: impl Into<String>, sk: impl Into<String>) -> Result<Option<Item>> {
        let output = self
            .client
            .get_item()
            .table_name(&self.table)
            .set_key(Some(key(pk, sk)))
            .consistent_read(true)
            .send()
            .await
            .map_err(fault)?;
        Ok(output.item)
    }

    /// The items of a partition, ascending by sort key: all of them, or those between two keys.
    async fn query(&self, pk: impl Into<String>, range: Option<(&str, &str)>) -> Result<Vec<Item>> {
        let pk = pk.into();
        let (condition, expression_values) = match range {
            Some((first, last)) => (
                "pk = :pk AND sk BETWEEN :first AND :last",
                values([(":pk", s(&pk)), (":first", s(first)), (":last", s(last))]),
            ),
            None => ("pk = :pk", values([(":pk", s(&pk))])),
        };
        let mut items = Vec::new();
        let mut resume = None;
        loop {
            let output = self
                .client
                .query()
                .table_name(&self.table)
                .key_condition_expression(condition)
                .set_expression_attribute_values(expression_values.clone())
                .consistent_read(true)
                .set_exclusive_start_key(resume)
                .send()
                .await
                .map_err(fault)?;
            items.extend(output.items.unwrap_or_default());
            resume = output.last_evaluated_key;
            if resume.is_none() {
                return Ok(items);
            }
        }
    }

    /// Every item of a partition whose sort key starts with `prefix`, ascending.
    async fn query_prefix(&self, pk: impl Into<String>, prefix: &str) -> Result<Vec<Item>> {
        // U+10FFFF sorts after every character a key can hold, in UTF-8 as in code points.
        self.query(pk, Some((prefix, &format!("{prefix}\u{10FFFF}")))).await
    }

    /// The items with the given keys, in no particular order. Missing ones are left out.
    async fn batch_get(&self, keys: Vec<Item>) -> Result<Vec<Item>> {
        let mut found = Vec::new();
        for chunk in keys.chunks(BATCH) {
            let mut pending = chunk.to_vec();
            let mut attempt = 0;
            while !pending.is_empty() {
                let request = KeysAndAttributes::builder()
                    .set_keys(Some(pending))
                    .consistent_read(true)
                    .build()
                    .map_err(fault)?;
                let output =
                    self.client.batch_get_item().request_items(&self.table, request).send().await.map_err(fault)?;
                if let Some(mut responses) = output.responses {
                    found.extend(responses.remove(&self.table).unwrap_or_default());
                }
                pending = output
                    .unprocessed_keys
                    .and_then(|mut all| all.remove(&self.table))
                    .map(|k| k.keys)
                    .unwrap_or_default();
                if !pending.is_empty() {
                    attempt += 1;
                    if attempt > ATTEMPTS {
                        return Err(Error::unavailable("batch read kept being cut short"));
                    }
                    back_off(attempt).await;
                }
            }
        }
        Ok(found)
    }

    fn put(&self, item: Item) -> aws_sdk_dynamodb::types::builders::PutBuilder {
        Put::builder().table_name(&self.table).set_item(Some(item))
    }

    fn put_new(&self, item: Item) -> TransactWriteItem {
        let put = self.put(item).condition_expression("attribute_not_exists(pk)").build().expect("table and item set");
        TransactWriteItem::builder().put(put).build()
    }

    fn put_any(&self, item: Item) -> TransactWriteItem {
        TransactWriteItem::builder().put(self.put(item).build().expect("table and item set")).build()
    }

    fn delete(&self, key: Item) -> TransactWriteItem {
        let delete = Delete::builder().table_name(&self.table).set_key(Some(key)).build().expect("table and key set");
        TransactWriteItem::builder().delete(delete).build()
    }

    fn delete_if(
        &self,
        key: Item,
        condition: &str,
        condition_values: Option<HashMap<String, Av>>,
    ) -> TransactWriteItem {
        let delete = Delete::builder()
            .table_name(&self.table)
            .set_key(Some(key))
            .condition_expression(condition)
            .set_expression_attribute_values(condition_values)
            .build()
            .expect("table and key set");
        TransactWriteItem::builder().delete(delete).build()
    }

    fn require(&self, key: Item) -> TransactWriteItem {
        let check = ConditionCheck::builder()
            .table_name(&self.table)
            .set_key(Some(key))
            .condition_expression("attribute_exists(pk)")
            .build()
            .expect("table, key and condition set");
        TransactWriteItem::builder().condition_check(check).build()
    }

    fn update(
        &self,
        key: Item,
        update: &str,
        condition: &str,
        expression_values: Option<HashMap<String, Av>>,
    ) -> TransactWriteItem {
        let update = Update::builder()
            .table_name(&self.table)
            .set_key(Some(key))
            .update_expression(update)
            .condition_expression(condition)
            .set_expression_attribute_values(expression_values)
            .build()
            .expect("table, key and expression set");
        TransactWriteItem::builder().update(update).build()
    }

    /// Runs a transaction. `None` when it took effect; otherwise, for each item in order, whether
    /// its condition was what stopped it.
    async fn transact(&self, items: Vec<TransactWriteItem>) -> Result<Option<Vec<bool>>> {
        for attempt in 0..ATTEMPTS {
            let sent = self.client.transact_write_items().set_transact_items(Some(items.clone())).send().await;
            let error = match sent {
                Ok(_) => return Ok(None),
                Err(error) => error,
            };
            let codes: Vec<&str> = match error.as_service_error() {
                Some(TransactWriteItemsError::TransactionCanceledException(cancelled)) => {
                    cancelled.cancellation_reasons().iter().map(|reason| reason.code().unwrap_or("None")).collect()
                }
                Some(TransactWriteItemsError::TransactionInProgressException(_)) => vec!["TransactionConflict"],
                _ => return Err(fault(error)),
            };
            if codes.contains(&"ConditionalCheckFailed") {
                return Ok(Some(codes.iter().map(|code| *code == "ConditionalCheckFailed").collect()));
            }
            let busy = ["TransactionConflict", "ThrottlingError", "ProvisionedThroughputExceeded"];
            if !codes.iter().any(|code| busy.contains(code)) {
                return Err(fault(error));
            }
            back_off(attempt).await;
        }
        Err(Error::unavailable("transaction kept colliding with other writers"))
    }

    /// Changes one item outside a transaction. `None` when the condition did not hold; otherwise
    /// the item as it is afterwards.
    async fn update_item(
        &self,
        key: Item,
        update: &str,
        condition: &str,
        expression_values: Option<HashMap<String, Av>>,
    ) -> Result<Option<Item>> {
        for attempt in 0..ATTEMPTS {
            let sent = self
                .client
                .update_item()
                .table_name(&self.table)
                .set_key(Some(key.clone()))
                .update_expression(update)
                .condition_expression(condition)
                .set_expression_attribute_values(expression_values.clone())
                .return_values(ReturnValue::AllNew)
                .send()
                .await;
            match sent {
                Ok(output) => return Ok(Some(output.attributes.unwrap_or_default())),
                Err(error) if condition_failed!(error) => return Ok(None),
                Err(error) if collided!(error) => back_off(attempt).await,
                Err(error) => return Err(fault(error)),
            }
        }
        Err(Error::unavailable("update kept colliding with other writers"))
    }

    async fn require_account(&self, id: AccountId) -> Result<Item> {
        self.get(account_pk(id), META).await?.ok_or(Error::NotFound)
    }

    async fn require_mailbox(&self, id: MailboxId) -> Result<Mailbox> {
        mailbox_from(&self.get(mailbox_pk(id), META).await?.ok_or(Error::NotFound)?)
    }

    /// Moves the mailbox's modification sequence on by one and returns the new value.
    async fn next_modseq(&self, mailbox: MailboxId) -> Result<ModSeq> {
        let updated = self
            .update_item(
                key(mailbox_pk(mailbox), META),
                "ADD highest_modseq :one",
                "attribute_exists(pk)",
                values([(":one", n(1))]),
            )
            .await?
            .ok_or(Error::NotFound)?;
        Ok(ModSeq(number(&updated, "highest_modseq")?))
    }

    /// Removes one record and gives up its reference to its blob. `None` when the record was
    /// gone already; otherwise whether the blob has no references left.
    async fn drop_record(&self, mailbox: MailboxId, record: &MessageRecord) -> Result<Option<bool>> {
        let refused = self
            .transact(vec![
                self.delete_if(key(mailbox_pk(mailbox), message_sk(record.uid)), "attribute_exists(pk)", None),
                self.update(
                    key(blob_pk(record.blob), META),
                    "ADD ref_count :minus_one",
                    "attribute_exists(pk)",
                    values([(":minus_one", n(-1))]),
                ),
            ])
            .await?;
        if refused.is_some() {
            return Ok(None);
        }
        // Mark the blob released, unless someone still points at it. The marker makes a late
        // append of the same blob fail instead of pointing at an object about to be deleted.
        let expires = epoch_seconds(SystemTime::now() + RELEASED_MARKER_LIFETIME);
        let released = self
            .update_item(
                key(blob_pk(record.blob), META),
                "SET is_released = :yes, expires_at = :expires",
                "ref_count <= :zero AND attribute_not_exists(is_released)",
                values([(":yes", Av::Bool(true)), (":expires", n(expires)), (":zero", n(0))]),
            )
            .await?;
        Ok(Some(released.is_some()))
    }
}

// ---------------------------------------------------------------------------------------------
// The trait
// ---------------------------------------------------------------------------------------------

#[async_trait]
impl MetadataStore for DynamoMetadataStore {
    async fn create_account(&self, new: NewAccount) -> Result<Account> {
        let account = Account {
            id: AccountId::generate(),
            primary_address: new.primary_address,
            display_name: new.display_name,
            state: AccountState::Active,
            created: SystemTime::now(),
        };
        let pk = account_pk(account.id);
        let pattern = AddressPattern::Exact(account.primary_address.clone());
        let refused = self
            .transact(vec![
                self.put_new(item(
                    address_pk(&pattern),
                    META,
                    [("account_id", s(account.id.to_string())), ("is_primary", Av::Bool(true))],
                )),
                self.put_any(item(
                    &pk,
                    META,
                    [
                        ("primary_address", s(account.primary_address.as_str())),
                        ("display_name", s(&account.display_name)),
                        ("account_state", state_value(account.state)),
                        ("created_at", time_value(account.created)),
                    ],
                )),
                self.put_any(key(&pk, address_sk(&pattern))),
                self.put_any(item(
                    ACCOUNTS,
                    account.primary_address.as_str(),
                    [("account_id", s(account.id.to_string()))],
                )),
            ])
            .await?;
        match refused {
            None => Ok(account),
            Some(_) => Err(Error::AlreadyExists),
        }
    }

    async fn get_account(&self, id: AccountId) -> Result<Account> {
        account_from(&self.require_account(id).await?)
    }

    async fn find_account_by_login(&self, login: &EmailAddress) -> Result<Option<Account>> {
        let Some(owner) = self.get(address_pk(&AddressPattern::Exact(login.clone())), META).await? else {
            return Ok(None);
        };
        if !flag(&owner, "is_primary") {
            return Ok(None);
        }
        match self.get_account(parsed(&owner, "account_id")?).await {
            Ok(account) => Ok(Some(account)),
            Err(Error::NotFound) => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn list_accounts(&self) -> Result<Vec<Account>> {
        let directory = self.query(ACCOUNTS, None).await?;
        let mut keys = Vec::with_capacity(directory.len());
        for entry in &directory {
            keys.push(key(account_pk(parsed(entry, "account_id")?), META));
        }
        let mut accounts = self.batch_get(keys).await?.iter().map(account_from).collect::<Result<Vec<_>>>()?;
        accounts.sort_by(|a, b| a.primary_address.cmp(&b.primary_address));
        Ok(accounts)
    }

    async fn set_account_state(&self, id: AccountId, state: AccountState) -> Result<()> {
        self.update_item(
            key(account_pk(id), META),
            "SET account_state = :state",
            "attribute_exists(pk)",
            values([(":state", state_value(state))]),
        )
        .await?
        .map(|_| ())
        .ok_or(Error::NotFound)
    }

    async fn delete_account(&self, id: AccountId) -> Result<()> {
        let account = self.get_account(id).await?;
        let pk = account_pk(id);
        let owned = self.query(&pk, None).await?;
        let mut deletes = vec![self.delete(key(ACCOUNTS, account.primary_address.as_str()))];
        for entry in &owned {
            let sk = text(entry, "sk")?;
            if sk.starts_with("MBOXNAME#") {
                return Err(Error::Conflict("account still has mailboxes"));
            }
            if let Some(pattern) = sk.strip_prefix("ADDR#") {
                deletes.push(self.delete(key(format!("ADDR#{pattern}"), META)));
            }
            // The account itself goes last, so that a failure half way leaves something to retry on.
            if sk != META {
                deletes.push(self.delete(key(&pk, sk)));
            }
        }
        deletes.push(self.delete(key(&pk, META)));
        for chunk in deletes.chunks(BATCH) {
            self.transact(chunk.to_vec()).await?;
        }
        Ok(())
    }

    async fn add_credential(&self, account: AccountId, new: NewCredential) -> Result<Credential> {
        let credential = Credential {
            id: CredentialId::generate(),
            label: new.label,
            secret_hash: new.secret_hash,
            created: SystemTime::now(),
            last_used: None,
        };
        let pk = account_pk(account);
        let stored = item(
            &pk,
            credential_sk(credential.id),
            [
                ("cred_label", s(&credential.label)),
                ("secret_hash", s(&credential.secret_hash)),
                ("created_at", time_value(credential.created)),
            ],
        );
        match self.transact(vec![self.require(key(&pk, META)), self.put_any(stored)]).await? {
            None => Ok(credential),
            Some(_) => Err(Error::NotFound),
        }
    }

    async fn list_credentials(&self, account: AccountId) -> Result<Vec<Credential>> {
        self.require_account(account).await?;
        let items = self.query_prefix(account_pk(account), "CRED#").await?;
        let mut credentials = items.iter().map(credential_from).collect::<Result<Vec<_>>>()?;
        credentials.sort_by_key(|credential| (credential.created, credential.id));
        Ok(credentials)
    }

    async fn touch_credential(&self, account: AccountId, credential: CredentialId, at: SystemTime) -> Result<()> {
        self.update_item(
            key(account_pk(account), credential_sk(credential)),
            "SET last_used = :at",
            "attribute_exists(pk)",
            values([(":at", time_value(at))]),
        )
        .await?
        .map(|_| ())
        .ok_or(Error::NotFound)
    }

    async fn remove_credential(&self, account: AccountId, credential: CredentialId) -> Result<()> {
        let target = key(account_pk(account), credential_sk(credential));
        match self.transact(vec![self.delete_if(target, "attribute_exists(pk)", None)]).await? {
            None => Ok(()),
            Some(_) => Err(Error::NotFound),
        }
    }

    async fn put_address(&self, pattern: AddressPattern, account: AccountId) -> Result<()> {
        let pk = account_pk(account);
        let refused = self
            .transact(vec![
                self.require(key(&pk, META)),
                self.update(
                    key(address_pk(&pattern), META),
                    "SET account_id = :account",
                    "attribute_not_exists(pk) OR account_id = :account",
                    values([(":account", s(account.to_string()))]),
                ),
                self.put_any(key(&pk, address_sk(&pattern))),
            ])
            .await?;
        match refused {
            None => Ok(()),
            Some(reasons) if reasons[0] => Err(Error::NotFound),
            Some(_) => Err(Error::AlreadyExists),
        }
    }

    async fn get_address(&self, pattern: &AddressPattern) -> Result<Option<AccountId>> {
        match self.get(address_pk(pattern), META).await? {
            Some(owner) => Ok(Some(parsed(&owner, "account_id")?)),
            None => Ok(None),
        }
    }

    async fn list_addresses(&self, account: AccountId) -> Result<Vec<AddressPattern>> {
        self.require_account(account).await?;
        let items = self.query_prefix(account_pk(account), "ADDR#").await?;
        items.iter().map(|entry| id_after(entry, "sk", "ADDR#")).collect()
    }

    async fn remove_address(&self, pattern: &AddressPattern) -> Result<()> {
        let owner = self.get(address_pk(pattern), META).await?.ok_or(Error::NotFound)?;
        if flag(&owner, "is_primary") {
            return Err(Error::Conflict("the primary address cannot be removed"));
        }
        let account: AccountId = parsed(&owner, "account_id")?;
        let refused = self
            .transact(vec![
                self.delete_if(
                    key(address_pk(pattern), META),
                    "account_id = :account",
                    values([(":account", s(account.to_string()))]),
                ),
                self.delete(key(account_pk(account), address_sk(pattern))),
            ])
            .await?;
        match refused {
            None => Ok(()),
            Some(_) => Err(Error::NotFound),
        }
    }

    async fn create_mailbox(
        &self,
        account: AccountId,
        name: MailboxName,
        role: Option<MailboxRole>,
    ) -> Result<Mailbox> {
        // A counter on the account hands out UIDVALIDITY values. A value is used at most once per
        // account, so no mailbox of this account ever had the one taken here, under any name.
        let counted = self
            .update_item(
                key(account_pk(account), META),
                "ADD validity_counter :one",
                "attribute_exists(pk)",
                values([(":one", n(1))]),
            )
            .await?
            .ok_or(Error::NotFound)?;
        let uid_validity = std::num::NonZeroU32::new(number(&counted, "validity_counter")?)
            .ok_or(Error::Conflict("out of UIDVALIDITY values"))?;
        let mailbox = Mailbox {
            id: MailboxId::generate(),
            account,
            name,
            role,
            subscribed: true,
            uid_validity: UidValidity(uid_validity),
            next_uid: Uid::FIRST,
            highest_modseq: ModSeq::default(),
        };
        let mut stored = item(
            mailbox_pk(mailbox.id),
            META,
            [
                ("account_id", s(account.to_string())),
                ("mailbox_name", s(mailbox.name.as_str())),
                ("is_subscribed", Av::Bool(true)),
                ("uid_validity", n(uid_validity)),
                ("next_uid", n(mailbox.next_uid.get())),
                ("highest_modseq", n(mailbox.highest_modseq.0)),
            ],
        );
        if let Some(role) = role {
            let name = ROLES.iter().find(|(known, _)| *known == role).expect("every role has a name").1;
            stored.insert("mailbox_role".to_owned(), s(name));
        }
        let reservation =
            item(account_pk(account), name_sk(&mailbox.name), [("mailbox_id", s(mailbox.id.to_string()))]);
        match self.transact(vec![self.put_new(reservation), self.put_any(stored)]).await? {
            None => Ok(mailbox),
            Some(_) => Err(Error::AlreadyExists),
        }
    }

    async fn get_mailbox(&self, id: MailboxId) -> Result<Mailbox> {
        self.require_mailbox(id).await
    }

    async fn find_mailbox(&self, account: AccountId, name: &MailboxName) -> Result<Option<Mailbox>> {
        let Some(reservation) = self.get(account_pk(account), name_sk(name)).await? else {
            // Only a miss pays for telling "no such mailbox" from "no such account".
            self.require_account(account).await?;
            return Ok(None);
        };
        match self.require_mailbox(parsed(&reservation, "mailbox_id")?).await {
            Ok(mailbox) => Ok(Some(mailbox)),
            Err(Error::NotFound) => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn list_mailboxes(&self, account: AccountId) -> Result<Vec<Mailbox>> {
        self.require_account(account).await?;
        let reservations = self.query_prefix(account_pk(account), "MBOXNAME#").await?;
        let mut keys = Vec::with_capacity(reservations.len());
        for reservation in &reservations {
            keys.push(key(mailbox_pk(parsed(reservation, "mailbox_id")?), META));
        }
        let mut mailboxes = self.batch_get(keys).await?.iter().map(mailbox_from).collect::<Result<Vec<_>>>()?;
        mailboxes.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(mailboxes)
    }

    async fn rename_mailbox(&self, id: MailboxId, new_name: MailboxName) -> Result<Mailbox> {
        let mailbox = self.require_mailbox(id).await?;
        let account_pk = account_pk(mailbox.account);
        let this = values([(":mailbox", s(id.to_string()))]);
        let refused = self
            .transact(vec![
                self.put_new(item(&account_pk, name_sk(&new_name), [("mailbox_id", s(id.to_string()))])),
                self.delete_if(key(&account_pk, name_sk(&mailbox.name)), "mailbox_id = :mailbox", this),
                self.update(
                    key(mailbox_pk(id), META),
                    "SET mailbox_name = :new",
                    "mailbox_name = :old",
                    values([(":new", s(new_name.as_str())), (":old", s(mailbox.name.as_str()))]),
                ),
            ])
            .await?;
        match refused {
            None => Ok(Mailbox { name: new_name, ..mailbox }),
            Some(reasons) if reasons[0] => Err(Error::AlreadyExists),
            // Renamed or deleted by someone else between the read and the write.
            Some(_) => Err(Error::NotFound),
        }
    }

    async fn set_subscribed(&self, id: MailboxId, subscribed: bool) -> Result<()> {
        self.update_item(
            key(mailbox_pk(id), META),
            "SET is_subscribed = :subscribed",
            "attribute_exists(pk)",
            values([(":subscribed", Av::Bool(subscribed))]),
        )
        .await?
        .map(|_| ())
        .ok_or(Error::NotFound)
    }

    async fn delete_mailbox(&self, id: MailboxId) -> Result<Vec<BlobId>> {
        let mailbox = self.require_mailbox(id).await?;
        let this = values([(":mailbox", s(id.to_string()))]);
        let refused = self
            .transact(vec![
                self.delete_if(key(account_pk(mailbox.account), name_sk(&mailbox.name)), "mailbox_id = :mailbox", this),
                self.delete_if(key(mailbox_pk(id), META), "attribute_exists(pk)", None),
            ])
            .await?;
        if refused.is_some() {
            return Err(Error::NotFound);
        }
        let mut released = Vec::new();
        for stored in self.query_prefix(mailbox_pk(id), "MSG#").await? {
            let record = message_from(id, &stored)?;
            if self.drop_record(id, &record).await? == Some(true) {
                released.push(record.blob);
            }
        }
        Ok(released)
    }

    async fn append_message(&self, mailbox: MailboxId, new: NewMessage) -> Result<MessageRecord> {
        let taken = self
            .update_item(
                key(mailbox_pk(mailbox), META),
                "ADD next_uid :one, highest_modseq :one",
                "attribute_exists(pk) AND next_uid < :last",
                values([(":one", n(1)), (":last", n(Uid::MAX.get()))]),
            )
            .await?;
        let Some(counters) = taken else {
            self.require_mailbox(mailbox).await?;
            return Err(Error::Conflict("mailbox has run out of UIDs"));
        };
        let following: u32 = number(&counters, "next_uid")?;
        let record = MessageRecord {
            mailbox,
            uid: Uid::new(following - 1).ok_or_else(|| malformed("next_uid"))?,
            blob: new.blob,
            size: new.size,
            internal_date: new.internal_date,
            flags: new.flags,
            modseq: ModSeq(number(&counters, "highest_modseq")?),
        };
        let mut stored = item(
            mailbox_pk(mailbox),
            message_sk(record.uid),
            [
                ("blob_id", s(record.blob.to_string())),
                ("byte_size", n(record.size)),
                ("internal_date", time_value(record.internal_date)),
                ("flag_bits", n(record.flags.system.bits())),
                ("mod_seq", n(record.modseq.0)),
            ],
        );
        if let Some(keywords) = keyword_value(&record.flags) {
            stored.insert("keyword_set".to_owned(), keywords);
        }
        let mut writes = vec![
            self.put_new(stored),
            self.update(
                key(blob_pk(record.blob), META),
                "ADD ref_count :one",
                "attribute_not_exists(is_released)",
                values([(":one", n(1))]),
            ),
        ];
        if let Some(once) = &new.once {
            // The delivery's key goes in with the record: both are stored, or neither.
            let now = SystemTime::now();
            let mark = item(
                format!("INGEST#{}", once.key),
                META,
                [("expires_at", n(epoch_seconds(now + once.remember_for).max(1)))],
            );
            let put = self
                .put(mark)
                .condition_expression("attribute_not_exists(pk) OR expires_at <= :now")
                .set_expression_attribute_values(values([(":now", n(epoch_seconds(now)))]))
                .build()
                .expect("table and item set");
            writes.push(TransactWriteItem::builder().put(put).build());
        }
        match self.transact(writes).await? {
            None => Ok(record),
            Some(reasons) if reasons.get(2) == Some(&true) => Err(Error::AlreadyExists),
            Some(reasons) if reasons[1] => Err(Error::Conflict("the blob was released already")),
            Some(_) => Err(Error::unavailable("a UID was handed out twice")),
        }
    }

    async fn messages(&self, mailbox: MailboxId, range: UidRange) -> Result<Vec<MessageRecord>> {
        self.require_mailbox(mailbox).await?;
        if range.first > range.last {
            return Ok(Vec::new());
        }
        let items = self.query(mailbox_pk(mailbox), Some((&message_sk(range.first), &message_sk(range.last)))).await?;
        items.iter().map(|stored| message_from(mailbox, stored)).collect()
    }

    async fn update_flags(&self, mailbox: MailboxId, uid: Uid, change: FlagChange) -> Result<MessageRecord> {
        let target = key(mailbox_pk(mailbox), message_sk(uid));
        for attempt in 0..ATTEMPTS {
            let stored = self.get(mailbox_pk(mailbox), message_sk(uid)).await?.ok_or(Error::NotFound)?;
            let mut record = message_from(mailbox, &stored)?;
            let read_at = record.modseq;
            if !record.flags.apply(&change) {
                return Ok(record);
            }
            record.modseq = self.next_modseq(mailbox).await?;
            let mut expression_values = HashMap::from([
                (":flags".to_owned(), n(record.flags.system.bits())),
                (":modseq".to_owned(), n(record.modseq.0)),
                (":read_at".to_owned(), n(read_at.0)),
            ]);
            let expression = match keyword_value(&record.flags) {
                Some(keywords) => {
                    expression_values.insert(":keywords".to_owned(), keywords);
                    "SET flag_bits = :flags, mod_seq = :modseq, keyword_set = :keywords"
                }
                None => "SET flag_bits = :flags, mod_seq = :modseq REMOVE keyword_set",
            };
            // Only if nobody changed the message since it was read; otherwise read again and redo.
            let written =
                self.update_item(target.clone(), expression, "mod_seq = :read_at", Some(expression_values)).await?;
            if written.is_some() {
                return Ok(record);
            }
            back_off(attempt).await;
        }
        Err(Error::unavailable("flag update kept colliding with other writers"))
    }

    async fn expunge(&self, mailbox: MailboxId, uids: &[Uid]) -> Result<Expunged> {
        self.require_mailbox(mailbox).await?;
        let wanted: BTreeSet<Uid> = uids.iter().copied().collect();
        let keys = wanted.iter().map(|uid| key(mailbox_pk(mailbox), message_sk(*uid))).collect();
        let mut records = self
            .batch_get(keys)
            .await?
            .iter()
            .map(|stored| message_from(mailbox, stored))
            .collect::<Result<Vec<_>>>()?;
        records.sort_by_key(|record| record.uid);

        let mut expunged = Expunged::default();
        for record in &records {
            if let Some(blob_released) = self.drop_record(mailbox, record).await? {
                expunged.uids.push(record.uid);
                if blob_released {
                    expunged.released_blobs.push(record.blob);
                }
            }
        }
        if !expunged.uids.is_empty() {
            self.next_modseq(mailbox).await?;
        }
        Ok(expunged)
    }
}
