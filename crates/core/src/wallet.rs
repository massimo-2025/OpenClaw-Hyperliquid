use anyhow::{Context, Result};
use ethers::core::k256::ecdsa::SigningKey;
use ethers::signers::{LocalWallet, Signer, Wallet};
use ethers::types::{Address, Signature, H256};
use ethers::utils::keccak256;
use std::str::FromStr;

/// Wrapper around an Ethereum wallet for signing Hyperliquid actions.
#[derive(Clone)]
pub struct HyperliquidWallet {
    wallet: LocalWallet,
    address: Address,
}

impl HyperliquidWallet {
    /// Create a wallet from a hex private key string.
    pub fn from_private_key(key: &str) -> Result<Self> {
        let key = key.strip_prefix("0x").unwrap_or(key);
        let wallet: Wallet<SigningKey> = key
            .parse::<LocalWallet>()
            .context("Failed to parse private key")?;
        let address = wallet.address();
        Ok(Self { wallet, address })
    }

    /// Get the wallet address as a hex string.
    pub fn address_string(&self) -> String {
        format!("{:?}", self.address)
    }

    /// Get the wallet address.
    pub fn address(&self) -> Address {
        self.address
    }

    /// Sign an arbitrary message (EIP-191 personal sign).
    pub async fn sign_message(&self, message: &[u8]) -> Result<Signature> {
        self.wallet
            .sign_message(message)
            .await
            .context("Failed to sign message")
    }

    /// Sign a typed data hash (for Hyperliquid L1 actions).
    pub async fn sign_typed_data_hash(&self, hash: H256) -> Result<Signature> {
        let sig = self
            .wallet
            .sign_hash(hash)
            .context("Failed to sign typed data hash")?;
        Ok(sig)
    }

    /// Compute keccak256 hash of data.
    pub fn keccak256(data: &[u8]) -> [u8; 32] {
        keccak256(data)
    }

    /// Sign a Hyperliquid action. The action is serialized as JSON, hashed,
    /// then signed with the wallet's private key.
    pub async fn sign_action(&self, action_json: &str) -> Result<SignedAction> {
        let hash = Self::keccak256(action_json.as_bytes());
        let sig = self
            .sign_typed_data_hash(H256::from(hash))
            .await?;
        Ok(SignedAction {
            action: action_json.to_string(),
            signature: format!("{sig}"),
            vault_address: None,
        })
    }

    /// Verify an address matches this wallet.
    pub fn verify_address(&self, addr: &str) -> bool {
        let addr = addr.strip_prefix("0x").unwrap_or(addr);
        let expected = format!("{:?}", self.address);
        let expected = expected.strip_prefix("0x").unwrap_or(&expected);
        addr.to_lowercase() == expected.to_lowercase()
    }
}

impl std::fmt::Debug for HyperliquidWallet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HyperliquidWallet")
            .field("address", &self.address_string())
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct SignedAction {
    pub action: String,
    pub signature: String,
    pub vault_address: Option<String>,
}

/// Parse an Ethereum address from a hex string.
pub fn parse_address(addr: &str) -> Result<Address> {
    let addr = addr.strip_prefix("0x").unwrap_or(addr);
    Address::from_str(addr).context("Invalid Ethereum address")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test key - NOT a real key, just for testing
    const TEST_PRIVATE_KEY: &str =
        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    #[test]
    fn test_wallet_creation() {
        let wallet = HyperliquidWallet::from_private_key(TEST_PRIVATE_KEY).unwrap();
        let addr = wallet.address_string();
        assert!(addr.starts_with("0x"));
        assert_eq!(addr.len(), 42);
    }

    #[test]
    fn test_wallet_creation_without_prefix() {
        let key = TEST_PRIVATE_KEY.strip_prefix("0x").unwrap();
        let wallet = HyperliquidWallet::from_private_key(key).unwrap();
        assert!(wallet.address_string().starts_with("0x"));
    }

    #[test]
    fn test_address_verification() {
        let wallet = HyperliquidWallet::from_private_key(TEST_PRIVATE_KEY).unwrap();
        let addr = wallet.address_string();
        assert!(wallet.verify_address(&addr));
        assert!(!wallet.verify_address("0x0000000000000000000000000000000000000000"));
    }

    #[tokio::test]
    async fn test_sign_message() {
        let wallet = HyperliquidWallet::from_private_key(TEST_PRIVATE_KEY).unwrap();
        let sig = wallet.sign_message(b"test message").await.unwrap();
        assert_ne!(sig.r, ethers::types::U256::zero());
    }

    #[tokio::test]
    async fn test_sign_action() {
        let wallet = HyperliquidWallet::from_private_key(TEST_PRIVATE_KEY).unwrap();
        let signed = wallet.sign_action(r#"{"type":"order"}"#).await.unwrap();
        assert!(!signed.signature.is_empty());
    }

    #[test]
    fn test_keccak256() {
        let hash = HyperliquidWallet::keccak256(b"hello");
        assert_eq!(hash.len(), 32);
    }

    #[test]
    fn test_parse_address() {
        let addr = parse_address("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266").unwrap();
        assert_ne!(addr, Address::zero());
    }

    #[test]
    fn test_parse_address_invalid() {
        assert!(parse_address("not-an-address").is_err());
    }
}
