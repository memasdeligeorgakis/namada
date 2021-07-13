use anoma_vm_env::vp_prelude::intent::{
    Exchange, FungibleTokenIntent, IntentTransfers,
};
use anoma_vm_env::vp_prelude::key::ed25519::{Signed, SignedTxData};
use anoma_vm_env::vp_prelude::*;
use rust_decimal::prelude::*;

enum KeyType<'a> {
    Token(&'a Address),
    InvalidIntentSet(&'a Address),
    Unknown,
}

impl<'a> From<&'a storage::Key> for KeyType<'a> {
    fn from(key: &'a storage::Key) -> KeyType<'a> {
        if let Some(address) = token::is_any_token_balance_key(key) {
            Self::Token(address)
        } else if let Some(address) = intent::is_invalid_intent_key(key) {
            Self::InvalidIntentSet(address)
        } else {
            Self::Unknown
        }
    }
}

#[validity_predicate]
fn validate_tx(
    tx_data: Vec<u8>,
    addr: Address,
    keys_changed: HashSet<storage::Key>,
    verifiers: HashSet<Address>,
) -> bool {
    log_string(format!(
        "validate_tx called with user addr: {}, key_changed: {:#?}, \
         verifiers: {:?}",
        addr, keys_changed, verifiers
    ));

    // TODO memoize?
    let valid_sig = match SignedTxData::try_from_slice(&tx_data[..]) {
        Ok(tx) => {
            let pk = key::ed25519::get(&addr);
            match pk {
                None => false,
                Some(pk) => verify_tx_signature(&pk, &tx.sig),
            }
        }
        _ => false,
    };

    // TODO memoize?
    // TODO this is not needed for matchmaker, maybe we should have a different
    // VP?
    let valid_intent = check_intent_transfers(&addr, &tx_data[..]);

    for key in keys_changed.iter() {
        let is_valid = match KeyType::from(key) {
            KeyType::Token(owner) if owner == &addr => {
                let key = key.to_string();
                let pre: token::Amount = read_pre(&key).unwrap_or_default();
                let post: token::Amount = read_post(&key).unwrap_or_default();
                let change = post.change() - pre.change();
                log_string(format!(
                    "token key: {}, change: {}, valid_sig: {}, valid_intent: \
                     {}, valid modification: {}",
                    key,
                    change,
                    valid_sig,
                    valid_intent,
                    (change < 0 && (valid_sig || valid_intent)) || change > 0
                ));
                // debit has to signed, credit doesn't
                (change < 0 && (valid_sig || valid_intent)) || change > 0
            }
            KeyType::InvalidIntentSet(owner) if owner == &addr => {
                let key = key.to_string();
                let pre: Vec<Vec<u8>> = read_pre(&key).unwrap_or_default();
                let post: Vec<Vec<u8>> = read_post(&key).unwrap_or_default();
                // only one sig is added, intent is already checked
                log_string(format!(
                    "intent sig set key: {}, valid modification: {}",
                    key,
                    pre.len() + 1 != post.len()
                ));
                pre.len() + 1 == post.len()
            }
            KeyType::Token(_owner) | KeyType::InvalidIntentSet(_owner) => {
                log_string(format!(
                    "key {} is not of owner, valid_sig {}",
                    key, valid_sig
                ));
                valid_sig
            }
            KeyType::Unknown => {
                log_string(format!(
                    "Unknown key modified, valid sig {}",
                    valid_sig
                ));
                valid_sig
            }
        };
        if !is_valid {
            log_string(format!("key {} modification failed vp", key));
            return false;
        }
    }
    true
}

fn check_intent_transfers(addr: &Address, tx_data: &[u8]) -> bool {
    match SignedTxData::try_from_slice(tx_data) {
        Ok(tx) => {
            match IntentTransfers::try_from_slice(&tx.data.unwrap()[..]) {
                Ok(tx_data) => {
                    if let Some(exchange) = &tx_data.exchanges.get(addr) {
                        let intent_data = &tx_data.intents.get(addr).expect(
                            "It should never fail since if there is an \
                             exchange with a specific address there must be a \
                             linked fungibletokenintent.",
                        );
                        log_string("check intent".to_string());
                        check_intent(addr, exchange, intent_data)
                    } else {
                        log_string(
                            "no intent with a matching address".to_string(),
                        );
                        false
                    }
                }
                Err(_) => false,
            }
        }
        Err(_) => false,
    }
}

fn check_intent(
    addr: &Address,
    exchange: &Signed<Exchange>,
    intent: &Signed<FungibleTokenIntent>,
) -> bool {
    // verify signature
    let pk = key::ed25519::get(addr);
    if let Some(pk) = pk {
        if intent.verify(&pk).is_err() {
            log_string("invalid sig".to_string());
            return false;
        }
    } else {
        return false;
    }

    // verify the intent have not been already used
    if !intent::vp_exchange(exchange) {
        return false;
    }

    // verify the intent is fulfilled
    let Exchange {
        addr: _,
        token_sell,
        rate_min,
        token_buy,
        amount_buy,
        max_sell,
    } = &exchange.data;

    let token_sell_key = token::balance_key(&token_sell, addr).to_string();
    let mut sell_difference: token::Amount =
        read_pre(&token_sell_key).unwrap_or_default();
    let sell_post: token::Amount =
        read_post(token_sell_key).unwrap_or_default();

    sell_difference.spend(&sell_post);

    let token_buy_key = token::balance_key(&token_buy, addr).to_string();
    let buy_pre: token::Amount = read_pre(&token_buy_key).unwrap_or_default();
    let mut buy_difference: token::Amount =
        read_post(token_buy_key).unwrap_or_default();

    buy_difference.spend(&buy_pre);

    let sell_diff: Decimal = sell_difference.change().into();
    let buy_diff: Decimal = buy_difference.change().into();

    // check if:
    // - buy_difference > 0 to avoid division by 0 and make sure that something
    //   is being sold/bought
    // - rate_min is respected
    // - max_sell is respected
    if buy_difference.change() <= 0
        || sell_diff / buy_diff > rate_min.0
        || max_sell.change() < sell_difference.change()
    {
        log_string(format!(
            "invalid exchange, {}, {}, {}",
            sell_difference.change(),
            buy_difference.change(),
            max_sell.change()
        ));
        false
    } else {
        true
    }
}

#[cfg(test)]
mod tests {
    use anoma_tests::vp::*;

    use super::*;

    /// Test that no-op transaction (i.e. no storage modifications) is deemed
    /// valid.
    #[test]
    fn test_no_op_transaction() {
        let mut env = TestVpEnv::default();
        init_vp_env(&mut env);

        let tx_data: Vec<u8> = vec![];
        let addr: Address = env.addr.clone();
        let keys_changed: HashSet<storage::Key> = HashSet::default();
        let verifiers: HashSet<Address> = HashSet::default();

        let valid = validate_tx(tx_data, addr, keys_changed, verifiers);

        assert!(valid);
    }
}
