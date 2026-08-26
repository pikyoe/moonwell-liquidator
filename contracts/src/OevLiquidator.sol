// SPDX-License-Identifier: MIT
pragma solidity ^0.8.19;

interface IMorpho {
    function flashLoan(address token, uint256 assets, bytes calldata data) external;
}

interface IERC20 {
    function balanceOf(address account) external view returns (uint256);
    function symbol() external view returns (string memory);
    function approve(address spender, uint256 amount) external returns (bool);
    function transfer(address to, uint256 amount) external returns (bool);
}

interface IOevWrapper {
    function updatePriceEarlyAndLiquidate(
        address borrower,
        uint256 repayAmount,
        address mTokenCollateral,
        address mTokenLoan
    ) external;
}

interface IMToken {
    function liquidateBorrow(address borrower, uint256 repayAmount, address mTokenCollateral)
        external
        returns (uint256);
    function underlying() external view returns (address);
    function balanceOf(address account) external view returns (uint256);
    function redeem(uint256 redeemTokens) external returns (uint256);
}

interface IOracle {
    function getFeed(string memory symbol) external view returns (address);
}

interface IComptroller {
    function oracle() external view returns (address);
}

enum Mode {
    Oev,     // Jalur A: Moonwell ChainlinkOEVWrapper
    Classic  // Jalur B: fallback liquidateBorrow setelah oracle update
}

struct LiquidationJob {
    Mode mode;
    IERC20 loanToken;              // underlying dari mTokenLoan (aset utang borrower)
    address swapTarget;            // router DEX opsional; address(0) = tanpa swap
    bytes swapData;                // calldata router untuk kolateral -> loanToken
    IMToken mTokenLoan;
    IMToken mTokenCollateral;
    address borrower;
    uint256 repayAmount;           // sudah dibatasi <= MAX_POSITION di off-chain
    uint256 minProfit;             // profit minimum collateral token (wei)
    uint256 minLoanOut;            // slippage guard swap: loan token minimal yang harus diterima
}

/// @notice Executor likuidasi Moonwell di Base, didanai flashloan Morpho (fee 0%).
/// Jalur A (Oev): ChainlinkOEVWrapper.updatePriceEarlyAndLiquidate sebelum oracle on-chain.
/// Jalur B (Classic): liquidateBorrow standar, siap dipakai sebagai fallback.
/// Bot off-chain selalu eth_call execute() dulu; revert = transaksi tidak dikirim.
contract OevLiquidator {
    IMorpho public constant morpho = IMorpho(0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb);
    IComptroller public constant comptroller = IComptroller(0xfBb21d0380beE3312B33c4353c8936a0F13EF26C);

    address public immutable owner;

    error NotProfitable(uint256 profit, uint256 minProfit);

    constructor() {
        owner = msg.sender;
    }

    modifier onlyOwner() {
        require(msg.sender == owner, "not owner");
        _;
    }

    function execute(LiquidationJob calldata job) external onlyOwner {
        require(job.repayAmount > 0, "zero repay");

        IERC20 collateralUnderlying = IERC20(IMToken(address(job.mTokenCollateral)).underlying());
        uint256 balBefore = collateralUnderlying.balanceOf(address(this));

        morpho.flashLoan(address(job.loanToken), job.repayAmount, abi.encode(job));

        uint256 profit = collateralUnderlying.balanceOf(address(this)) - balBefore;
        if (profit < job.minProfit) revert NotProfitable(profit, job.minProfit);
    }

    function onMorphoFlashLoan(uint256 assets, bytes calldata data) external {
        require(msg.sender == address(morpho), "bad caller");
        LiquidationJob memory job = abi.decode(data, (LiquidationJob));

        if (job.mode == Mode.Oev) {
            _oevLiquidate(job);
        } else {
            _classicLiquidate(job);
        }

        uint256 seized = job.mTokenCollateral.balanceOf(address(this));
        require(seized > 0, "zero seized");
        require(job.mTokenCollateral.redeem(seized) == 0, "redeem failed");

        IERC20 collateralUnderlying = IERC20(job.mTokenCollateral.underlying());

        // Swap opsional: konversi kolateral -> loanToken supaya flashloan tertutup.
        // Dilewati bila swapTarget == address(0) (mode tanpa swap).
        if (collateralUnderlying != job.loanToken && job.swapTarget != address(0)) {
            uint256 bal = collateralUnderlying.balanceOf(address(this));
            _approve(address(collateralUnderlying), job.swapTarget, bal);
            (bool ok, bytes memory ret) = job.swapTarget.call(job.swapData);
            require(ok && (ret.length == 0 || abi.decode(ret, (bool))), "swap failed");
        }

        // Pengembalian flashloan: Morpho menarik `assets` via transferFrom.
        // - Mode swap  : tertutup dari hasil swap.
        // - Tanpa swap : tertutup dari cadangan loanToken owner di kontrak ini,
        //                yang terisi kembali saat profit kolateral dijual off-chain.
        require(
            job.loanToken.balanceOf(address(this)) >= assets + job.minLoanOut,
            "loan token tidak cukup untuk repay flashloan"
        );
        _approve(address(job.loanToken), address(morpho), assets);
    }

    function _approve(address token, address spender, uint256 amount) internal {
        (bool ok, bytes memory ret) =
            token.call(abi.encodeWithSelector(IERC20.approve.selector, spender, amount));
        require(ok && (ret.length == 0 || abi.decode(ret, (bool))), "approve failed");
    }

    function _oevLiquidate(LiquidationJob memory job) internal {
        IERC20 collateralUnderlying = IERC20(job.mTokenCollateral.underlying());
        IOracle oracle = IOracle(comptroller.oracle());
        address wrapper = oracle.getFeed(collateralUnderlying.symbol());
        require(wrapper != address(0), "no wrapper");

        _approve(address(job.loanToken), wrapper, job.repayAmount);
        IOevWrapper(wrapper).updatePriceEarlyAndLiquidate(
            job.borrower,
            job.repayAmount,
            address(job.mTokenCollateral),
            address(job.mTokenLoan)
        );
    }

    function _classicLiquidate(LiquidationJob memory job) internal {
        _approve(address(job.loanToken), address(job.mTokenLoan), job.repayAmount);
        require(
            job.mTokenLoan.liquidateBorrow(
                job.borrower,
                job.repayAmount,
                address(job.mTokenCollateral)
            ) == 0,
            "liquidate failed"
        );
    }

    /// Owner menyedot token apa pun (profit kolateral, atau sisa cadangan)
    /// kapan pun. Profit disimpan di kontrak antar-eksekusi agar tidak
    /// menambah gas transfer di setiap likuidasi.
    function sweep(address token, uint256 amount) external onlyOwner {
        _transfer(token, owner, amount);
    }

    function _transfer(address token, address to, uint256 amount) internal {
        (bool ok, bytes memory ret) =
            token.call(abi.encodeWithSelector(IERC20.transfer.selector, to, amount));
        require(ok && (ret.length == 0 || abi.decode(ret, (bool))), "transfer failed");
    }
}
