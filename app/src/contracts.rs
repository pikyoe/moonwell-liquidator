use alloy::sol;

// ABI minimal untuk Moonwell (Compound V2 fork), Morpho, dan executor kita.
sol! {
    #[sol(rpc)]
    interface IMToken {
        function exchangeRateStored() external view returns (uint256);
        function exchangeRateCurrent() external returns (uint256);
        function borrowBalanceStored(address account) external view returns (uint256);
        /// Accrue-borrow balance untuk satu akun - non-view, memicu accrueInterest()
        /// on-chain. Dipanggil via eth_call (simulasi) sehingga TIDAK menulis state;
        /// memberi nilai borrow balance yang akurat-terakru (lihat audit staleness).
        function borrowBalanceCurrent(address account) external returns (uint256);
        function balanceOf(address account) external view returns (uint256);
        function balanceOfUnderlying(address account) external returns (uint256);
        function underlying() external view returns (address);
        function symbol() external view returns (string);
        function liquidateBorrow(address borrower, uint256 repayAmount, address mTokenCollateral) external returns (uint256);
        function borrowIndex() external view returns (uint256);
        function accrueInterest() external returns (uint256);
        function getAccountSnapshot(address account) external view returns (uint256 err, uint256 mTokenBalance, uint256 borrowBalance, uint256 exchangeRateMantissa);
        function protocolSeizeShareMantissa() external view returns (uint256);
        /// Timestamp blok terakhir kali market di-accrue (checkpoint akrual bunga.
        /// Sidik staleness: accrualBlockTimestamp != block.timestamp => snapshot
        /// getAccountSnapshot membawa nilai cached, bukan akrual terkini.
        /// accrualBlockTimestamp() memberi timestamp checkpoint; beda dengan block.timestamp
        /// menunjukkan snapshot belum di-accrue sampai blok saat ini.
        function accrualBlockTimestamp() external view returns (uint256);
    }
    /// Dipakai untuk batch keputusan staleness + accrue-fresh dalam SATU eth_call
    /// (Rekomendasi 4 - hemat RPC saat sweep kandidat mepet).
    #[sol(rpc)]
    interface IMulticall3 {
        struct Call3 {
            address target;
            bool allowFailure;
            bytes callData;
        }
        struct Result {
            bool success;
            bytes returnData;
        }
        function aggregate3(Call3[] calldata calls) external payable returns (Result[] memory returnData);
    }
    #[sol(rpc)]
    interface IComptroller {
        function getAccountLiquidity(address account) external view returns (uint256 err, uint256 liquidity, uint256 shortfall);
        function getAssetsIn(address account) external view returns (address[]);
        function markets(address mToken) external view returns (bool isListed, uint256 collateralFactorMantissa);
        function closeFactorMantissa() external view returns (uint256);
        function liquidationIncentiveMantissa() external view returns (uint256);
        function oracle() external view returns (address);
    }
    #[sol(rpc)]
    interface IChainlinkOEVWrapper {
        function updatePriceEarlyAndLiquidate(
            address borrower,
            uint256 repayAmount,
            address mTokenCollateral,
            address mTokenLoan
        ) external;
        function liquidatorFeeBps() external view returns (uint16);
    }
    #[sol(rpc)]
    interface IOracle {
        function getFeed(string symbol) external view returns (address);
        function getUnderlyingPrice(address mToken) external view returns (uint256);
    }
    #[sol(rpc)]
    interface IMorpho {
        function flashLoan(address token, uint256 assets, bytes data) external;
    }
    enum Mode {
        Oev,
        Classic
    }
    struct LiquidationJob {
        Mode mode;
        address loanToken;
        address swapTarget;
        bytes swapData;
        address mTokenLoan;
        address mTokenCollateral;
        address borrower;
        uint256 repayAmount;
        uint256 minProfit;
        uint256 minLoanOut;
    }
    #[sol(rpc)]
    interface IOevLiquidator {
        function execute(LiquidationJob job) external;
        function sweep(address token, uint256 amount) external;
        function owner() external view returns (address);
    }
    #[sol(rpc)]
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
        function symbol() external view returns (string);
        function decimals() external view returns (uint8);
    }
    // Aerodrome Router (Base) - venue swap default
    struct Route {
        address from;
        address to;
        bool stable;
        address factory;
    }

    #[sol(rpc)]
    interface IAerodromeRouter {
        function swapExactTokensForTokens(
            uint256 amountIn,
            uint256 amountOutMin,
            Route[] routes,
            address to,
            uint256 deadline
        ) external returns (uint256[] amounts);
        function factory() external view returns (address);
    }
}

pub const COMPTROLLER: alloy::primitives::Address = alloy::primitives::Address::new([
    0xFB, 0xB2, 0x1D, 0x03, 0x80, 0xBE, 0xE3, 0x31, 0x2B, 0x33, 0xC4, 0x35, 0x3C, 0x89, 0x36, 0xA0, 0xF1, 0x3E, 0xF2, 0x6C
]);

/// Multicall3 - deterministik di semua chain EVM (mcd.
pub const MULTICALL3: alloy::primitives::Address = alloy::primitives::Address::new([
    0xCA, 0x11, 0xBD, 0xE0, 0x59, 0x77, 0xB3, 0x63, 0x11, 0x67, 0x02, 0x88, 0x62, 0xBE, 0x2A, 0x17, 0x39, 0x76, 0xCA, 0x11
]);
