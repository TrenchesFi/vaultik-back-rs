use alloy::sol;

// This file contains the Solidity interfaces for interacting with the
// MetaMorpho vault and related contracts.  It was taken directly from
// the user's snippet and is reproduced here unchanged for completeness.

sol! {
    // MetaMorpho vault interface (only functions we need)
    #[sol(rpc)]
    contract MetaMorpho {
        function MORPHO() external view returns (address);
        function supplyQueueLength() external view returns (uint256);
        function supplyQueue(uint256 index) external view returns (bytes32);
        function withdrawQueueLength() external view returns (uint256);
        function withdrawQueue(uint256 index) external view returns (bytes32); // Id
        function fee() external view returns (uint96);
        function totalAssets() external view returns (uint256);
    }

    #[sol(rpc)]
    contract IMorpho {
        // types
        type Id is bytes32;

        #[derive(Debug)]
        struct MarketParams {
            address loanToken;
            address collateralToken;
            address oracle;
            address irm;
            uint256 lltv;
        }

        #[derive(Debug)]
        struct Market {
            uint128 totalSupplyAssets;
            uint128 totalSupplyShares;
            uint128 totalBorrowAssets;
            uint128 totalBorrowShares;
            uint128 lastUpdate;
            uint128 fee; // WAD (1e18)
        }
        struct Position {
            uint256 supplyShares;
            uint128 borrowShares;
            uint128 collateral;
        }

        // views we need
        function idToMarketParams(Id id) external view returns (MarketParams);
        function market(Id id) external view returns (Market);
        function position(Id id, address user) external view returns (Position);
    }

    #[sol(rpc)]
    interface IIrm {
        function borrowRateView(IMorpho.MarketParams marketParams, IMorpho.Market market) external view returns (uint256);
    }
}
