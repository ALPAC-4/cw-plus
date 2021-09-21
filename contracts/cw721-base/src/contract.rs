use cosmwasm_std::{
    to_binary, Addr, Binary, BlockInfo, Deps, DepsMut, Env, MessageInfo, Order, Pair, Response,
    StdError, StdResult, Storage,
};

use cw0::maybe_addr;
use cw2::set_contract_version;
use cw721::{
    AllNftInfoResponse, ApprovedForAllResponse, ContractInfoResponse, Cw721ReceiveMsg, Expiration,
    NftInfoResponse, NumTokensResponse, OwnerOfResponse, TokensResponse,
};

use crate::error::ContractError;
use crate::msg::{ExecuteMsg, InstantiateMsg, MintMsg, MinterResponse, QueryMsg};
use crate::state::{token_owner_idx, Approval, TokenIndexes, TokenInfo};
use cw_storage_plus::{Bound, IndexedMap, Item, Map, MultiIndex};
use schemars::JsonSchema;
use std::marker::PhantomData;

// version info for migration info
const CONTRACT_NAME: &str = "crates.io:cw721-base";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

const DEFAULT_LIMIT: u32 = 10;
const MAX_LIMIT: u32 = 30;

pub struct Cw721Contract<'a, C> {
    pub contract_info: Item<'a, ContractInfoResponse>,
    pub minter: Item<'a, Addr>,
    pub token_count: Item<'a, u64>,
    /// Stored as (granter, operator) giving operator full control over granter's account
    pub operators: Map<'a, (&'a Addr, &'a Addr), Expiration>,
    pub tokens: IndexedMap<'a, &'a str, TokenInfo, TokenIndexes<'a>>,

    _custom_response: PhantomData<C>,
}

impl<C> Default for Cw721Contract<'static, C> {
    fn default() -> Self {
        Self::new(
            "nft_info",
            "minter",
            "num_tokens",
            "operators",
            "tokens",
            "tokens__owner",
        )
    }
}

impl<'a, C> Cw721Contract<'a, C> {
    fn new(
        contract_key: &'a str,
        minter_key: &'a str,
        token_count_key: &'a str,
        operator_key: &'a str,
        tokens_key: &'a str,
        tokens_owner_key: &'a str,
    ) -> Self {
        let indexes = TokenIndexes {
            owner: MultiIndex::new(token_owner_idx, tokens_key, tokens_owner_key),
        };
        Self {
            contract_info: Item::new(contract_key),
            minter: Item::new(minter_key),
            token_count: Item::new(token_count_key),
            operators: Map::new(operator_key),
            tokens: IndexedMap::new(tokens_key, indexes),
            _custom_response: PhantomData,
        }
    }

    pub fn num_tokens(&self, storage: &dyn Storage) -> StdResult<u64> {
        Ok(self.token_count.may_load(storage)?.unwrap_or_default())
    }

    pub fn increment_tokens(&self, storage: &mut dyn Storage) -> StdResult<u64> {
        let val = self.num_tokens(storage)? + 1;
        self.token_count.save(storage, &val)?;
        Ok(val)
    }
}

impl<'a, C> Cw721Contract<'a, C>
where
    C: Clone + std::fmt::Debug + PartialEq + JsonSchema,
{
    pub fn instantiate(
        &self,
        deps: DepsMut,
        _env: Env,
        _info: MessageInfo,
        msg: InstantiateMsg,
    ) -> StdResult<Response<C>> {
        set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

        let info = ContractInfoResponse {
            name: msg.name,
            symbol: msg.symbol,
        };
        self.contract_info.save(deps.storage, &info)?;
        let minter = deps.api.addr_validate(&msg.minter)?;
        self.minter.save(deps.storage, &minter)?;
        Ok(Response::default())
    }

    pub fn execute(
        &self,
        deps: DepsMut,
        env: Env,
        info: MessageInfo,
        msg: ExecuteMsg,
    ) -> Result<Response<C>, ContractError> {
        match msg {
            ExecuteMsg::Mint(msg) => self.execute_mint(deps, env, info, msg),
            ExecuteMsg::Approve {
                spender,
                token_id,
                expires,
            } => self.execute_approve(deps, env, info, spender, token_id, expires),
            ExecuteMsg::Revoke { spender, token_id } => {
                self.execute_revoke(deps, env, info, spender, token_id)
            }
            ExecuteMsg::ApproveAll { operator, expires } => {
                self.execute_approve_all(deps, env, info, operator, expires)
            }
            ExecuteMsg::RevokeAll { operator } => {
                self.execute_revoke_all(deps, env, info, operator)
            }
            ExecuteMsg::TransferNft {
                recipient,
                token_id,
            } => self.execute_transfer_nft(deps, env, info, recipient, token_id),
            ExecuteMsg::SendNft {
                contract,
                token_id,
                msg,
            } => self.execute_send_nft(deps, env, info, contract, token_id, msg),
        }
    }

    pub fn execute_mint(
        &self,
        deps: DepsMut,
        _env: Env,
        info: MessageInfo,
        msg: MintMsg,
    ) -> Result<Response<C>, ContractError> {
        let minter = self.minter.load(deps.storage)?;

        if info.sender != minter {
            return Err(ContractError::Unauthorized {});
        }

        // create the token
        let token = TokenInfo {
            owner: deps.api.addr_validate(&msg.owner)?,
            approvals: vec![],
            name: msg.name,
            description: msg.description.unwrap_or_default(),
            image: msg.image,
        };
        self.tokens
            .update(deps.storage, &msg.token_id, |old| match old {
                Some(_) => Err(ContractError::Claimed {}),
                None => Ok(token),
            })?;

        self.increment_tokens(deps.storage)?;

        Ok(Response::new()
            .add_attribute("action", "mint")
            .add_attribute("minter", info.sender)
            .add_attribute("token_id", msg.token_id))
    }

    pub fn execute_transfer_nft(
        &self,
        deps: DepsMut,
        env: Env,
        info: MessageInfo,
        recipient: String,
        token_id: String,
    ) -> Result<Response<C>, ContractError> {
        self._transfer_nft(deps, &env, &info, &recipient, &token_id)?;

        Ok(Response::new()
            .add_attribute("action", "transfer_nft")
            .add_attribute("sender", info.sender)
            .add_attribute("recipient", recipient)
            .add_attribute("token_id", token_id))
    }

    pub fn execute_send_nft(
        &self,
        deps: DepsMut,
        env: Env,
        info: MessageInfo,
        contract: String,
        token_id: String,
        msg: Binary,
    ) -> Result<Response<C>, ContractError> {
        // Transfer token
        self._transfer_nft(deps, &env, &info, &contract, &token_id)?;

        let send = Cw721ReceiveMsg {
            sender: info.sender.to_string(),
            token_id: token_id.clone(),
            msg,
        };

        // Send message
        Ok(Response::new()
            .add_message(send.into_cosmos_msg(contract.clone())?)
            .add_attribute("action", "send_nft")
            .add_attribute("sender", info.sender)
            .add_attribute("recipient", contract)
            .add_attribute("token_id", token_id))
    }

    pub fn _transfer_nft(
        &self,
        deps: DepsMut,
        env: &Env,
        info: &MessageInfo,
        recipient: &str,
        token_id: &str,
    ) -> Result<TokenInfo, ContractError> {
        let mut token = self.tokens.load(deps.storage, &token_id)?;
        // ensure we have permissions
        self.check_can_send(deps.as_ref(), env, info, &token)?;
        // set owner and remove existing approvals
        token.owner = deps.api.addr_validate(recipient)?;
        token.approvals = vec![];
        self.tokens.save(deps.storage, &token_id, &token)?;
        Ok(token)
    }

    pub fn execute_approve(
        &self,
        deps: DepsMut,
        env: Env,
        info: MessageInfo,
        spender: String,
        token_id: String,
        expires: Option<Expiration>,
    ) -> Result<Response<C>, ContractError> {
        self._update_approvals(deps, &env, &info, &spender, &token_id, true, expires)?;

        Ok(Response::new()
            .add_attribute("action", "approve")
            .add_attribute("sender", info.sender)
            .add_attribute("spender", spender)
            .add_attribute("token_id", token_id))
    }

    pub fn execute_revoke(
        &self,
        deps: DepsMut,
        env: Env,
        info: MessageInfo,
        spender: String,
        token_id: String,
    ) -> Result<Response<C>, ContractError> {
        self._update_approvals(deps, &env, &info, &spender, &token_id, false, None)?;

        Ok(Response::new()
            .add_attribute("action", "revoke")
            .add_attribute("sender", info.sender)
            .add_attribute("spender", spender)
            .add_attribute("token_id", token_id))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn _update_approvals(
        &self,
        deps: DepsMut,
        env: &Env,
        info: &MessageInfo,
        spender: &str,
        token_id: &str,
        // if add == false, remove. if add == true, remove then set with this expiration
        add: bool,
        expires: Option<Expiration>,
    ) -> Result<TokenInfo, ContractError> {
        let mut token = self.tokens.load(deps.storage, &token_id)?;
        // ensure we have permissions
        self.check_can_approve(deps.as_ref(), env, info, &token)?;

        // update the approval list (remove any for the same spender before adding)
        let spender_addr = deps.api.addr_validate(spender)?;
        token.approvals = token
            .approvals
            .into_iter()
            .filter(|apr| apr.spender != spender_addr)
            .collect();

        // only difference between approve and revoke
        if add {
            // reject expired data as invalid
            let expires = expires.unwrap_or_default();
            if expires.is_expired(&env.block) {
                return Err(ContractError::Expired {});
            }
            let approval = Approval {
                spender: spender_addr,
                expires,
            };
            token.approvals.push(approval);
        }

        self.tokens.save(deps.storage, &token_id, &token)?;

        Ok(token)
    }

    pub fn execute_approve_all(
        &self,
        deps: DepsMut,
        env: Env,
        info: MessageInfo,
        operator: String,
        expires: Option<Expiration>,
    ) -> Result<Response<C>, ContractError> {
        // reject expired data as invalid
        let expires = expires.unwrap_or_default();
        if expires.is_expired(&env.block) {
            return Err(ContractError::Expired {});
        }

        // set the operator for us
        let operator_addr = deps.api.addr_validate(&operator)?;
        self.operators
            .save(deps.storage, (&info.sender, &operator_addr), &expires)?;

        Ok(Response::new()
            .add_attribute("action", "approve_all")
            .add_attribute("sender", info.sender)
            .add_attribute("operator", operator))
    }

    pub fn execute_revoke_all(
        &self,
        deps: DepsMut,
        _env: Env,
        info: MessageInfo,
        operator: String,
    ) -> Result<Response<C>, ContractError> {
        let operator_addr = deps.api.addr_validate(&operator)?;
        self.operators
            .remove(deps.storage, (&info.sender, &operator_addr));

        Ok(Response::new()
            .add_attribute("action", "revoke_all")
            .add_attribute("sender", info.sender)
            .add_attribute("operator", operator))
    }

    /// returns true iff the sender can execute approve or reject on the contract
    pub fn check_can_approve(
        &self,
        deps: Deps,
        env: &Env,
        info: &MessageInfo,
        token: &TokenInfo,
    ) -> Result<(), ContractError> {
        // owner can approve
        if token.owner == info.sender {
            return Ok(());
        }
        // operator can approve
        let op = self
            .operators
            .may_load(deps.storage, (&token.owner, &info.sender))?;
        match op {
            Some(ex) => {
                if ex.is_expired(&env.block) {
                    Err(ContractError::Unauthorized {})
                } else {
                    Ok(())
                }
            }
            None => Err(ContractError::Unauthorized {}),
        }
    }

    /// returns true iff the sender can transfer ownership of the token
    fn check_can_send(
        &self,
        deps: Deps,
        env: &Env,
        info: &MessageInfo,
        token: &TokenInfo,
    ) -> Result<(), ContractError> {
        // owner can send
        if token.owner == info.sender {
            return Ok(());
        }

        // any non-expired token approval can send
        if token
            .approvals
            .iter()
            .any(|apr| apr.spender == info.sender && !apr.is_expired(&env.block))
        {
            return Ok(());
        }

        // operator can send
        let op = self
            .operators
            .may_load(deps.storage, (&token.owner, &info.sender))?;
        match op {
            Some(ex) => {
                if ex.is_expired(&env.block) {
                    Err(ContractError::Unauthorized {})
                } else {
                    Ok(())
                }
            }
            None => Err(ContractError::Unauthorized {}),
        }
    }

    pub fn query(&self, deps: Deps, env: Env, msg: QueryMsg) -> StdResult<Binary> {
        match msg {
            QueryMsg::Minter {} => to_binary(&self.query_minter(deps)?),
            QueryMsg::ContractInfo {} => to_binary(&self.query_contract_info(deps)?),
            QueryMsg::NftInfo { token_id } => to_binary(&self.query_nft_info(deps, token_id)?),
            QueryMsg::OwnerOf {
                token_id,
                include_expired,
            } => to_binary(&self.query_owner_of(
                deps,
                env,
                token_id,
                include_expired.unwrap_or(false),
            )?),
            QueryMsg::AllNftInfo {
                token_id,
                include_expired,
            } => to_binary(&self.query_all_nft_info(
                deps,
                env,
                token_id,
                include_expired.unwrap_or(false),
            )?),
            QueryMsg::ApprovedForAll {
                owner,
                include_expired,
                start_after,
                limit,
            } => to_binary(&self.query_all_approvals(
                deps,
                env,
                owner,
                include_expired.unwrap_or(false),
                start_after,
                limit,
            )?),
            QueryMsg::NumTokens {} => to_binary(&self.query_num_tokens(deps)?),
            QueryMsg::Tokens {
                owner,
                start_after,
                limit,
            } => to_binary(&self.query_tokens(deps, owner, start_after, limit)?),
            QueryMsg::AllTokens { start_after, limit } => {
                to_binary(&self.query_all_tokens(deps, start_after, limit)?)
            }
        }
    }

    pub fn query_minter(&self, deps: Deps) -> StdResult<MinterResponse> {
        let minter_addr = self.minter.load(deps.storage)?;
        Ok(MinterResponse {
            minter: minter_addr.to_string(),
        })
    }

    pub fn query_contract_info(&self, deps: Deps) -> StdResult<ContractInfoResponse> {
        self.contract_info.load(deps.storage)
    }

    pub fn query_num_tokens(&self, deps: Deps) -> StdResult<NumTokensResponse> {
        let count = self.num_tokens(deps.storage)?;
        Ok(NumTokensResponse { count })
    }

    pub fn query_nft_info(&self, deps: Deps, token_id: String) -> StdResult<NftInfoResponse> {
        let info = self.tokens.load(deps.storage, &token_id)?;
        Ok(NftInfoResponse {
            name: info.name,
            description: info.description,
            image: info.image,
        })
    }

    pub fn query_owner_of(
        &self,
        deps: Deps,
        env: Env,
        token_id: String,
        include_expired: bool,
    ) -> StdResult<OwnerOfResponse> {
        let info = self.tokens.load(deps.storage, &token_id)?;
        Ok(OwnerOfResponse {
            owner: info.owner.to_string(),
            approvals: humanize_approvals(&env.block, &info, include_expired),
        })
    }

    pub fn query_all_approvals(
        &self,
        deps: Deps,
        env: Env,
        owner: String,
        include_expired: bool,
        start_after: Option<String>,
        limit: Option<u32>,
    ) -> StdResult<ApprovedForAllResponse> {
        let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize;
        let start_addr = maybe_addr(deps.api, start_after)?;
        let start = start_addr.map(|addr| Bound::exclusive(addr.as_ref()));

        let owner_addr = deps.api.addr_validate(&owner)?;
        let res: StdResult<Vec<_>> = self
            .operators
            .prefix(&owner_addr)
            .range(deps.storage, start, None, Order::Ascending)
            .filter(|r| {
                include_expired || r.is_err() || !r.as_ref().unwrap().1.is_expired(&env.block)
            })
            .take(limit)
            .map(parse_approval)
            .collect();
        Ok(ApprovedForAllResponse { operators: res? })
    }

    pub fn query_tokens(
        &self,
        deps: Deps,
        owner: String,
        start_after: Option<String>,
        limit: Option<u32>,
    ) -> StdResult<TokensResponse> {
        let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize;
        let start = start_after.map(Bound::exclusive);

        let owner_addr = deps.api.addr_validate(&owner)?;
        let pks: Vec<_> = self
            .tokens
            .idx
            .owner
            .prefix(owner_addr)
            .keys(deps.storage, start, None, Order::Ascending)
            .take(limit)
            .collect();

        let res: Result<Vec<_>, _> = pks.iter().map(|v| String::from_utf8(v.to_vec())).collect();
        let tokens = res.map_err(StdError::invalid_utf8)?;
        Ok(TokensResponse { tokens })
    }

    pub fn query_all_tokens(
        &self,
        deps: Deps,
        start_after: Option<String>,
        limit: Option<u32>,
    ) -> StdResult<TokensResponse> {
        let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize;
        let start_addr = maybe_addr(deps.api, start_after)?;
        let start = start_addr.map(|addr| Bound::exclusive(addr.as_ref()));

        let tokens: StdResult<Vec<String>> = self
            .tokens
            .range(deps.storage, start, None, Order::Ascending)
            .take(limit)
            .map(|item| item.map(|(k, _)| String::from_utf8_lossy(&k).to_string()))
            .collect();
        Ok(TokensResponse { tokens: tokens? })
    }

    pub fn query_all_nft_info(
        &self,
        deps: Deps,
        env: Env,
        token_id: String,
        include_expired: bool,
    ) -> StdResult<AllNftInfoResponse> {
        let info = self.tokens.load(deps.storage, &token_id)?;
        Ok(AllNftInfoResponse {
            access: OwnerOfResponse {
                owner: info.owner.to_string(),
                approvals: humanize_approvals(&env.block, &info, include_expired),
            },
            info: NftInfoResponse {
                name: info.name,
                description: info.description,
                image: info.image,
            },
        })
    }
}

fn parse_approval(item: StdResult<Pair<Expiration>>) -> StdResult<cw721::Approval> {
    item.and_then(|(k, expires)| {
        let spender = String::from_utf8(k)?;
        Ok(cw721::Approval { spender, expires })
    })
}

fn humanize_approvals(
    block: &BlockInfo,
    info: &TokenInfo,
    include_expired: bool,
) -> Vec<cw721::Approval> {
    info.approvals
        .iter()
        .filter(|apr| include_expired || !apr.is_expired(block))
        .map(humanize_approval)
        .collect()
}

fn humanize_approval(approval: &Approval) -> cw721::Approval {
    cw721::Approval {
        spender: approval.spender.to_string(),
        expires: approval.expires,
    }
}

#[cfg(test)]
mod tests {
    use cosmwasm_std::testing::{mock_dependencies, mock_env, mock_info};
    use cosmwasm_std::{from_binary, CosmosMsg, Empty, WasmMsg};

    use super::*;
    use cw721::ApprovedForAllResponse;

    const MINTER: &str = "merlin";
    const CONTRACT_NAME: &str = "Magic Power";
    const SYMBOL: &str = "MGK";

    fn setup_contract(deps: DepsMut<'_>) -> Cw721Contract<'static, Empty> {
        let contract = Cw721Contract::default();
        let msg = InstantiateMsg {
            name: CONTRACT_NAME.to_string(),
            symbol: SYMBOL.to_string(),
            minter: String::from(MINTER),
        };
        let info = mock_info("creator", &[]);
        let res = contract.instantiate(deps, mock_env(), info, msg).unwrap();
        assert_eq!(0, res.messages.len());
        contract
    }

    #[test]
    fn proper_instantiation() {
        let mut deps = mock_dependencies(&[]);
        let contract = Cw721Contract::<Empty>::default();

        let msg = InstantiateMsg {
            name: CONTRACT_NAME.to_string(),
            symbol: SYMBOL.to_string(),
            minter: String::from(MINTER),
        };
        let info = mock_info("creator", &[]);

        // we can just call .unwrap() to assert this was a success
        let res = contract
            .instantiate(deps.as_mut(), mock_env(), info, msg)
            .unwrap();
        assert_eq!(0, res.messages.len());

        // it worked, let's query the state
        let res = contract.query_minter(deps.as_ref()).unwrap();
        assert_eq!(MINTER, res.minter);
        let info = contract.query_contract_info(deps.as_ref()).unwrap();
        assert_eq!(
            info,
            ContractInfoResponse {
                name: CONTRACT_NAME.to_string(),
                symbol: SYMBOL.to_string(),
            }
        );

        let count = contract.query_num_tokens(deps.as_ref()).unwrap();
        assert_eq!(0, count.count);

        // list the token_ids
        let tokens = contract
            .query_all_tokens(deps.as_ref(), None, None)
            .unwrap();
        assert_eq!(0, tokens.tokens.len());
    }

    #[test]
    fn minting() {
        let mut deps = mock_dependencies(&[]);
        let contract = setup_contract(deps.as_mut());

        let token_id = "petrify".to_string();
        let name = "Petrify with Gaze".to_string();
        let description = "Allows the owner to petrify anyone looking at him or her".to_string();

        let mint_msg = ExecuteMsg::Mint(MintMsg {
            token_id: token_id.clone(),
            owner: String::from("medusa"),
            name: name.clone(),
            description: Some(description.clone()),
            image: None,
        });

        // random cannot mint
        let random = mock_info("random", &[]);
        let err = contract
            .execute(deps.as_mut(), mock_env(), random, mint_msg.clone())
            .unwrap_err();
        assert_eq!(err, ContractError::Unauthorized {});

        // minter can mint
        let allowed = mock_info(MINTER, &[]);
        let _ = contract
            .execute(deps.as_mut(), mock_env(), allowed, mint_msg)
            .unwrap();

        // ensure num tokens increases
        let count = contract.query_num_tokens(deps.as_ref()).unwrap();
        assert_eq!(1, count.count);

        // unknown nft returns error
        let _ = contract
            .query_nft_info(deps.as_ref(), "unknown".to_string())
            .unwrap_err();

        // this nft info is correct
        let info = contract
            .query_nft_info(deps.as_ref(), token_id.clone())
            .unwrap();
        assert_eq!(
            info,
            NftInfoResponse {
                name,
                description,
                image: None,
            }
        );

        // owner info is correct
        let owner = contract
            .query_owner_of(deps.as_ref(), mock_env(), token_id.clone(), true)
            .unwrap();
        assert_eq!(
            owner,
            OwnerOfResponse {
                owner: String::from("medusa"),
                approvals: vec![],
            }
        );

        // Cannot mint same token_id again
        let mint_msg2 = ExecuteMsg::Mint(MintMsg {
            token_id: token_id.clone(),
            owner: String::from("hercules"),
            name: "copy cat".into(),
            description: None,
            image: None,
        });

        let allowed = mock_info(MINTER, &[]);
        let err = contract
            .execute(deps.as_mut(), mock_env(), allowed, mint_msg2)
            .unwrap_err();
        assert_eq!(err, ContractError::Claimed {});

        // list the token_ids
        let tokens = contract
            .query_all_tokens(deps.as_ref(), None, None)
            .unwrap();
        assert_eq!(1, tokens.tokens.len());
        assert_eq!(vec![token_id], tokens.tokens);
    }

    #[test]
    fn transferring_nft() {
        let mut deps = mock_dependencies(&[]);
        let contract = setup_contract(deps.as_mut());

        // Mint a token
        let token_id = "melt".to_string();
        let name = "Melting power".to_string();
        let description = "Allows the owner to melt anyone looking at him or her".to_string();

        let mint_msg = ExecuteMsg::Mint(MintMsg {
            token_id: token_id.clone(),
            owner: String::from("venus"),
            name,
            description: Some(description),
            image: None,
        });

        let minter = mock_info(MINTER, &[]);
        contract
            .execute(deps.as_mut(), mock_env(), minter, mint_msg)
            .unwrap();

        // random cannot transfer
        let random = mock_info("random", &[]);
        let transfer_msg = ExecuteMsg::TransferNft {
            recipient: String::from("random"),
            token_id: token_id.clone(),
        };

        let err = contract
            .execute(deps.as_mut(), mock_env(), random, transfer_msg)
            .unwrap_err();
        assert_eq!(err, ContractError::Unauthorized {});

        // owner can
        let random = mock_info("venus", &[]);
        let transfer_msg = ExecuteMsg::TransferNft {
            recipient: String::from("random"),
            token_id: token_id.clone(),
        };

        let res = contract
            .execute(deps.as_mut(), mock_env(), random, transfer_msg)
            .unwrap();

        assert_eq!(
            res,
            Response::new()
                .add_attribute("action", "transfer_nft")
                .add_attribute("sender", "venus")
                .add_attribute("recipient", "random")
                .add_attribute("token_id", token_id)
        );
    }

    #[test]
    fn sending_nft() {
        let mut deps = mock_dependencies(&[]);
        let contract = setup_contract(deps.as_mut());

        // Mint a token
        let token_id = "melt".to_string();
        let name = "Melting power".to_string();
        let description = "Allows the owner to melt anyone looking at him or her".to_string();

        let mint_msg = ExecuteMsg::Mint(MintMsg {
            token_id: token_id.clone(),
            owner: String::from("venus"),
            name,
            description: Some(description),
            image: None,
        });

        let minter = mock_info(MINTER, &[]);
        contract
            .execute(deps.as_mut(), mock_env(), minter, mint_msg)
            .unwrap();

        let msg = to_binary("You now have the melting power").unwrap();
        let target = String::from("another_contract");
        let send_msg = ExecuteMsg::SendNft {
            contract: target.clone(),
            token_id: token_id.clone(),
            msg: msg.clone(),
        };

        let random = mock_info("random", &[]);
        let err = contract
            .execute(deps.as_mut(), mock_env(), random, send_msg.clone())
            .unwrap_err();
        assert_eq!(err, ContractError::Unauthorized {});

        // but owner can
        let random = mock_info("venus", &[]);
        let res = contract
            .execute(deps.as_mut(), mock_env(), random, send_msg)
            .unwrap();

        let payload = Cw721ReceiveMsg {
            sender: String::from("venus"),
            token_id: token_id.clone(),
            msg,
        };
        let expected = payload.into_cosmos_msg(target.clone()).unwrap();
        // ensure expected serializes as we think it should
        match &expected {
            CosmosMsg::Wasm(WasmMsg::Execute { contract_addr, .. }) => {
                assert_eq!(contract_addr, &target)
            }
            m => panic!("Unexpected message type: {:?}", m),
        }
        // and make sure this is the request sent by the contract
        assert_eq!(
            res,
            Response::new()
                .add_message(expected)
                .add_attribute("action", "send_nft")
                .add_attribute("sender", "venus")
                .add_attribute("recipient", "another_contract")
                .add_attribute("token_id", token_id)
        );
    }

    #[test]
    fn approving_revoking() {
        let mut deps = mock_dependencies(&[]);
        let contract = setup_contract(deps.as_mut());

        // Mint a token
        let token_id = "grow".to_string();
        let name = "Growing power".to_string();
        let description = "Allows the owner to grow anything".to_string();

        let mint_msg = ExecuteMsg::Mint(MintMsg {
            token_id: token_id.clone(),
            owner: String::from("demeter"),
            name,
            description: Some(description),
            image: None,
        });

        let minter = mock_info(MINTER, &[]);
        contract
            .execute(deps.as_mut(), mock_env(), minter, mint_msg)
            .unwrap();

        // Give random transferring power
        let approve_msg = ExecuteMsg::Approve {
            spender: String::from("random"),
            token_id: token_id.clone(),
            expires: None,
        };
        let owner = mock_info("demeter", &[]);
        let res = contract
            .execute(deps.as_mut(), mock_env(), owner, approve_msg)
            .unwrap();
        assert_eq!(
            res,
            Response::new()
                .add_attribute("action", "approve")
                .add_attribute("sender", "demeter")
                .add_attribute("spender", "random")
                .add_attribute("token_id", token_id.clone())
        );

        // random can now transfer
        let random = mock_info("random", &[]);
        let transfer_msg = ExecuteMsg::TransferNft {
            recipient: String::from("person"),
            token_id: token_id.clone(),
        };
        contract
            .execute(deps.as_mut(), mock_env(), random, transfer_msg)
            .unwrap();

        // Approvals are removed / cleared
        let query_msg = QueryMsg::OwnerOf {
            token_id: token_id.clone(),
            include_expired: None,
        };
        let res: OwnerOfResponse = from_binary(
            &contract
                .query(deps.as_ref(), mock_env(), query_msg.clone())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            res,
            OwnerOfResponse {
                owner: String::from("person"),
                approvals: vec![],
            }
        );

        // Approve, revoke, and check for empty, to test revoke
        let approve_msg = ExecuteMsg::Approve {
            spender: String::from("random"),
            token_id: token_id.clone(),
            expires: None,
        };
        let owner = mock_info("person", &[]);
        contract
            .execute(deps.as_mut(), mock_env(), owner.clone(), approve_msg)
            .unwrap();

        let revoke_msg = ExecuteMsg::Revoke {
            spender: String::from("random"),
            token_id,
        };
        contract
            .execute(deps.as_mut(), mock_env(), owner, revoke_msg)
            .unwrap();

        // Approvals are now removed / cleared
        let res: OwnerOfResponse = from_binary(
            &contract
                .query(deps.as_ref(), mock_env(), query_msg)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            res,
            OwnerOfResponse {
                owner: String::from("person"),
                approvals: vec![],
            }
        );
    }

    #[test]
    fn approving_all_revoking_all() {
        let mut deps = mock_dependencies(&[]);
        let contract = setup_contract(deps.as_mut());

        // Mint a couple tokens (from the same owner)
        let token_id1 = "grow1".to_string();
        let name1 = "Growing power".to_string();
        let description1 = "Allows the owner the power to grow anything".to_string();
        let token_id2 = "grow2".to_string();
        let name2 = "More growing power".to_string();
        let description2 = "Allows the owner the power to grow anything even faster".to_string();

        let mint_msg1 = ExecuteMsg::Mint(MintMsg {
            token_id: token_id1.clone(),
            owner: String::from("demeter"),
            name: name1,
            description: Some(description1),
            image: None,
        });

        let minter = mock_info(MINTER, &[]);
        contract
            .execute(deps.as_mut(), mock_env(), minter.clone(), mint_msg1)
            .unwrap();

        let mint_msg2 = ExecuteMsg::Mint(MintMsg {
            token_id: token_id2.clone(),
            owner: String::from("demeter"),
            name: name2,
            description: Some(description2),
            image: None,
        });

        contract
            .execute(deps.as_mut(), mock_env(), minter, mint_msg2)
            .unwrap();

        // paginate the token_ids
        let tokens = contract
            .query_all_tokens(deps.as_ref(), None, Some(1))
            .unwrap();
        assert_eq!(1, tokens.tokens.len());
        assert_eq!(vec![token_id1.clone()], tokens.tokens);
        let tokens = contract
            .query_all_tokens(deps.as_ref(), Some(token_id1.clone()), Some(3))
            .unwrap();
        assert_eq!(1, tokens.tokens.len());
        assert_eq!(vec![token_id2.clone()], tokens.tokens);

        // demeter gives random full (operator) power over her tokens
        let approve_all_msg = ExecuteMsg::ApproveAll {
            operator: String::from("random"),
            expires: None,
        };
        let owner = mock_info("demeter", &[]);
        let res = contract
            .execute(deps.as_mut(), mock_env(), owner, approve_all_msg)
            .unwrap();
        assert_eq!(
            res,
            Response::new()
                .add_attribute("action", "approve_all")
                .add_attribute("sender", "demeter")
                .add_attribute("operator", "random")
        );

        // random can now transfer
        let random = mock_info("random", &[]);
        let transfer_msg = ExecuteMsg::TransferNft {
            recipient: String::from("person"),
            token_id: token_id1,
        };
        contract
            .execute(deps.as_mut(), mock_env(), random.clone(), transfer_msg)
            .unwrap();

        // random can now send
        let inner_msg = WasmMsg::Execute {
            contract_addr: "another_contract".into(),
            msg: to_binary("You now also have the growing power").unwrap(),
            funds: vec![],
        };
        let msg: CosmosMsg = CosmosMsg::Wasm(inner_msg);

        let send_msg = ExecuteMsg::SendNft {
            contract: String::from("another_contract"),
            token_id: token_id2,
            msg: to_binary(&msg).unwrap(),
        };
        contract
            .execute(deps.as_mut(), mock_env(), random, send_msg)
            .unwrap();

        // Approve_all, revoke_all, and check for empty, to test revoke_all
        let approve_all_msg = ExecuteMsg::ApproveAll {
            operator: String::from("operator"),
            expires: None,
        };
        // person is now the owner of the tokens
        let owner = mock_info("person", &[]);
        contract
            .execute(deps.as_mut(), mock_env(), owner, approve_all_msg)
            .unwrap();

        let res = contract
            .query_all_approvals(
                deps.as_ref(),
                mock_env(),
                String::from("person"),
                true,
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            res,
            ApprovedForAllResponse {
                operators: vec![cw721::Approval {
                    spender: String::from("operator"),
                    expires: Expiration::Never {}
                }]
            }
        );

        // second approval
        let buddy_expires = Expiration::AtHeight(1234567);
        let approve_all_msg = ExecuteMsg::ApproveAll {
            operator: String::from("buddy"),
            expires: Some(buddy_expires),
        };
        let owner = mock_info("person", &[]);
        contract
            .execute(deps.as_mut(), mock_env(), owner.clone(), approve_all_msg)
            .unwrap();

        // and paginate queries
        let res = contract
            .query_all_approvals(
                deps.as_ref(),
                mock_env(),
                String::from("person"),
                true,
                None,
                Some(1),
            )
            .unwrap();
        assert_eq!(
            res,
            ApprovedForAllResponse {
                operators: vec![cw721::Approval {
                    spender: String::from("buddy"),
                    expires: buddy_expires,
                }]
            }
        );
        let res = contract
            .query_all_approvals(
                deps.as_ref(),
                mock_env(),
                String::from("person"),
                true,
                Some(String::from("buddy")),
                Some(2),
            )
            .unwrap();
        assert_eq!(
            res,
            ApprovedForAllResponse {
                operators: vec![cw721::Approval {
                    spender: String::from("operator"),
                    expires: Expiration::Never {}
                }]
            }
        );

        let revoke_all_msg = ExecuteMsg::RevokeAll {
            operator: String::from("operator"),
        };
        contract
            .execute(deps.as_mut(), mock_env(), owner, revoke_all_msg)
            .unwrap();

        // Approvals are removed / cleared without affecting others
        let res = contract
            .query_all_approvals(
                deps.as_ref(),
                mock_env(),
                String::from("person"),
                false,
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            res,
            ApprovedForAllResponse {
                operators: vec![cw721::Approval {
                    spender: String::from("buddy"),
                    expires: buddy_expires,
                }]
            }
        );

        // ensure the filter works (nothing should be here
        let mut late_env = mock_env();
        late_env.block.height = 1234568; //expired
        let res = contract
            .query_all_approvals(
                deps.as_ref(),
                late_env,
                String::from("person"),
                false,
                None,
                None,
            )
            .unwrap();
        assert_eq!(0, res.operators.len());
    }

    #[test]
    fn query_tokens_by_owner() {
        let mut deps = mock_dependencies(&[]);
        let contract = setup_contract(deps.as_mut());
        let minter = mock_info(MINTER, &[]);

        // Mint a couple tokens (from the same owner)
        let token_id1 = "grow1".to_string();
        let demeter = String::from("Demeter");
        let token_id2 = "grow2".to_string();
        let ceres = String::from("Ceres");
        let token_id3 = "sing".to_string();

        let mint_msg = ExecuteMsg::Mint(MintMsg {
            token_id: token_id1.clone(),
            owner: demeter.clone(),
            name: "Growing power".to_string(),
            description: Some("Allows the owner the power to grow anything".to_string()),
            image: None,
        });
        contract
            .execute(deps.as_mut(), mock_env(), minter.clone(), mint_msg)
            .unwrap();

        let mint_msg = ExecuteMsg::Mint(MintMsg {
            token_id: token_id2.clone(),
            owner: ceres.clone(),
            name: "More growing power".to_string(),
            description: Some(
                "Allows the owner the power to grow anything even faster".to_string(),
            ),
            image: None,
        });
        contract
            .execute(deps.as_mut(), mock_env(), minter.clone(), mint_msg)
            .unwrap();

        let mint_msg = ExecuteMsg::Mint(MintMsg {
            token_id: token_id3.clone(),
            owner: demeter.clone(),
            name: "Sing a lullaby".to_string(),
            description: Some("Calm even the most excited children".to_string()),
            image: None,
        });
        contract
            .execute(deps.as_mut(), mock_env(), minter, mint_msg)
            .unwrap();

        // get all tokens in order:
        let expected = vec![token_id1.clone(), token_id2.clone(), token_id3.clone()];
        let tokens = contract
            .query_all_tokens(deps.as_ref(), None, None)
            .unwrap();
        assert_eq!(&expected, &tokens.tokens);
        // paginate
        let tokens = contract
            .query_all_tokens(deps.as_ref(), None, Some(2))
            .unwrap();
        assert_eq!(&expected[..2], &tokens.tokens[..]);
        let tokens = contract
            .query_all_tokens(deps.as_ref(), Some(expected[1].clone()), None)
            .unwrap();
        assert_eq!(&expected[2..], &tokens.tokens[..]);

        // get by owner
        let by_ceres = vec![token_id2];
        let by_demeter = vec![token_id1, token_id3];
        // all tokens by owner
        let tokens = contract
            .query_tokens(deps.as_ref(), demeter.clone(), None, None)
            .unwrap();
        assert_eq!(&by_demeter, &tokens.tokens);
        let tokens = contract
            .query_tokens(deps.as_ref(), ceres, None, None)
            .unwrap();
        assert_eq!(&by_ceres, &tokens.tokens);

        // paginate for demeter
        let tokens = contract
            .query_tokens(deps.as_ref(), demeter.clone(), None, Some(1))
            .unwrap();
        assert_eq!(&by_demeter[..1], &tokens.tokens[..]);
        let tokens = contract
            .query_tokens(deps.as_ref(), demeter, Some(by_demeter[0].clone()), Some(3))
            .unwrap();
        assert_eq!(&by_demeter[1..], &tokens.tokens[..]);
    }
}
