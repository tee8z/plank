use bdk_wallet::bitcoin::{Amount, Denomination, SignedAmount};

pub fn format_amount(amount: &Amount) -> String {
    format!("{} sats", amount.display_in(Denomination::SAT))
}

pub fn format_signed_amount(amount: &SignedAmount) -> String {
    format!("{} sats", amount.display_in(Denomination::SAT))
}
