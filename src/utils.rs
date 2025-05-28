use bdk_wallet::bitcoin::{Amount, Denomination, OutPoint, SignedAmount, Txid};

pub fn format_amount(amount: &Amount) -> String {
    format!("{} sats", amount.display_in(Denomination::SAT))
}

pub fn format_signed_amount(amount: &SignedAmount) -> String {
    format!("{} sats", amount.display_in(Denomination::SAT))
}

pub fn short_tx_id(tx_id: &Txid) -> String {
    let s = tx_id.to_string();
    if s.len() <= 30 {
        // If the string is short, just display it as is
        s
    } else {
        // Otherwise, show first 15 and last 15 characters with ... in between
        format!("{}...{}", &s[..15], &s[s.len() - 15..])
    }
}

pub fn short_outpoint(outpoint: &OutPoint) -> String {
    let txid = short_tx_id(&outpoint.txid);
    format!("{}:{}", txid, outpoint.vout)
}
