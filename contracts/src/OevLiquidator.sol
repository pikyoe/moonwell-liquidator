// SPDX-License-Identifier: MIT
pragma solidity ^0.8.19;

import {SafeERC20} from "openzeppelin-contracts/contracts/token/ERC20/utils/SafeERC20.sol";
import {IERC20} from "openzeppelin-contracts/contracts/token/ERC20/IERC20.sol";

interface IMorpho {
    function flashLoan(address token, uint256 assets, bytes calldata data) external;
}

interface IERC20Symbol {
    function symbol() external view returns (string memory);
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
    using SafeERC20 for IERC20;

    IMorpho public constant morpho = IMorpho(0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb);
    IComptroller public constant comptroller = IComptroller(0xfBb21d0380beE3312B33c4353c8936a0F13EF26C);

    address public immutable owner;

    /// Hash job yang sah untuk callback flashloan berikutnya.
    /// Hanya execute() (onlyOwner) yang boleh mengaturnya. Callback hanya
    /// jalan bila ID cocok; di-clear setelahnya. Tanpa ini, siapa pun bisa
    /// memanggil morpho.flashLoan dengan kontrak ini sebagai penerima dan
    /// mencuri reserve via swapTarget/swapData arbitrer.
    bytes32 public expectedCallHash;

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

        // Profit diukur di token hasil akhir: loanToken bila swap aktif,
        // kolateral underlying bila tidak (termasuk kasus token sama).
        IERC20 collateralUnderlying = IERC20(IMToken(address(job.mTokenCollateral)).underlying());
        bool swapExpected = job.swapTarget != address(0) && address(collateralUnderlying) != address(job.loanToken);
        IERC20 profitToken = swapExpected ? job.loanToken : collateralUnderlying;

        uint256 balBefore = profitToken.balanceOf(address(this));

        expectedCallHash = keccak256(abi.encode(job));
        morpho.flashLoan(address(job.loanToken), job.repayAmount, abi.encode(job));

        uint256 profit = profitToken.balanceOf(address(this)) - balBefore;
        expectedCallHash = bytes32(0);
        // Reset allowance Morpho yang diset di callback.
        job.loanToken.forceApprove(address(morpho), 0);
        if (profit < job.minProfit) revert NotProfitable(profit, job.minProfit);
    }

    function onMorphoFlashLoan(uint256 assets, bytes calldata data) external {
        require(msg.sender == address(morpho), "bad caller");
        LiquidationJob memory job = abi.decode(data, (LiquidationJob));

        // Hanya terima callback untuk flashloan yang dipicu execute().
        // Hash tidak di-clear di sini — kalau callback revert, flag di execute()
        // juga revert sehingga tidak sisa stuck. Clear di execute() setelah selesai.
        require(expectedCallHash == keccak256(abi.encode(job)), "unknown flashloan");

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
            uint256 loanBalBefore = job.loanToken.balanceOf(address(this));
            uint256 bal = collateralUnderlying.balanceOf(address(this));
            collateralUnderlying.forceApprove(job.swapTarget, bal);
            (bool ok,) = job.swapTarget.call(job.swapData);
            require(ok, "swap failed");
            // Verifikasi lewat delta saldo, bukan decode return value.
            require(
                job.loanToken.balanceOf(address(this)) > loanBalBefore,
                "swap tidak mengembalikan apa pun"
            );
            collateralUnderlying.forceApprove(job.swapTarget, 0);
        }

        // Pengembalian flashloan: Morpho menarik `assets` via transferFrom.
        // - Mode swap  : tertutup dari hasil swap.
        // - Tanpa swap : tertutup dari cadangan loanToken owner di kontrak ini,
        //                yang terisi kembali saat profit kolateral dijual off-chain.
        require(
            job.loanToken.balanceOf(address(this)) >= assets + job.minLoanOut,
            "loan token tidak cukup untuk repay flashloan"
        );
        job.loanToken.forceApprove(address(morpho), assets);
    }

    function _oevLiquidate(LiquidationJob memory job) internal {
        IERC20 collateralUnderlying = IERC20(job.mTokenCollateral.underlying());
        IOracle oracle = IOracle(comptroller.oracle());
        address wrapper = oracle.getFeed(IERC20Symbol(address(collateralUnderlying)).symbol());
        require(wrapper != address(0), "no wrapper");

        job.loanToken.forceApprove(wrapper, job.repayAmount);
        IOevWrapper(wrapper).updatePriceEarlyAndLiquidate(
            job.borrower,
            job.repayAmount,
            address(job.mTokenCollateral),
            address(job.mTokenLoan)
        );
        // Hapus sisa allowance ke wrapper agar tidak ada approval permanen.
        job.loanToken.forceApprove(wrapper, 0);
    }

    function _classicLiquidate(LiquidationJob memory job) internal {
        job.loanToken.forceApprove(address(job.mTokenLoan), job.repayAmount);
        require(
            job.mTokenLoan.liquidateBorrow(
                job.borrower,
                job.repayAmount,
                address(job.mTokenCollateral)
            ) == 0,
            "liquidate failed"
        );
        job.loanToken.forceApprove(address(job.mTokenLoan), 0);
    }

    /// Owner menyedot token apa pun (profit kolateral, atau sisa cadangan)
    /// kapan pun. Profit disimpan di kontrak antar-eksekusi agar tidak
    /// menambah gas transfer di setiap likuidasi.
    function sweep(address token, uint256 amount) external onlyOwner {
        IERC20(token).safeTransfer(owner, amount);
    }
}
