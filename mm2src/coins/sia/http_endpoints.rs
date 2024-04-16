use crate::sia::address::Address;
use mm2_number::MmNumber;

// TODO this can be H256 type instead of String ie `use rpc::v1::types::H256;`
// requires custom serde because the walletd API displays it like:
// "id": "bid:0079148b08cd64112de2cfccbd0f2b4d5a40c618726665349a8954d1c463b03b"
pub type BlockId = String; 

#[derive(Deserialize, Serialize, Debug)]
pub enum SiaApiRequest {
    ConsensusTip(ConsensusTipRequest),
    AddressBalance(AddressBalanceRequest),
}

#[derive(Deserialize, Serialize, Debug)]
pub enum SiaApiResponse {
    ConsensusTip(ConsensusTipResponse),
    AddressBalance(AddressBalanceResponse)
}

#[derive(Deserialize, Serialize, Debug)]
pub struct ConsensusTipRequest;

// https://github.com/SiaFoundation/core/blob/4e46803f702891e7a83a415b7fcd7543b13e715e/types/types.go#L181
#[derive(Deserialize, Serialize, Debug)]
pub struct ConsensusTipResponse {
    pub height: u64,
    pub id: String, // TODO this can match "BlockID" type
}

#[derive(Deserialize, Serialize, Debug)]
pub struct AddressBalanceRequest {
    pub address: Address,
}

// https://github.com/SiaFoundation/walletd/blob/9574e69ff0bf84de1235b68e78db2a41d5e27516/api/api.go#L36
// https://github.com/SiaFoundation/walletd/blob/9574e69ff0bf84de1235b68e78db2a41d5e27516/wallet/wallet.go#L25
#[derive(Deserialize, Serialize, Debug)]
pub struct AddressBalanceResponse {
    pub siacoins: MmNumber,
    #[serde(rename = "immatureSiacoins")]
    pub immature_siacoins: MmNumber,
    pub siafunds: u64,
}

