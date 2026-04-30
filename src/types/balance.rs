use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Balance {
    pub currency_code: String,
    pub amount: Decimal,
    pub available: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub product_code: String,
    pub side: String,
    pub price: Decimal,
    pub size: Decimal,
    pub commission: Decimal,
    pub sfd: Decimal,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn balance_json_round_trip() {
        let balance = Balance {
            currency_code: "JPY".to_string(),
            amount: dec!(1000000),
            available: dec!(900000),
        };
        let json = serde_json::to_string(&balance).unwrap();
        let decoded: Balance = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.currency_code, balance.currency_code);
        assert_eq!(decoded.amount, balance.amount);
    }
}
