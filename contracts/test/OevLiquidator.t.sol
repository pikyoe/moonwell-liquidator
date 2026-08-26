// SPDX-License-Identifier: MIT
pragma solidity ^0.8.19;

import "forge-std/Test.sol";
import "../src/OevLiquidator.sol";

interface IComptrollerFull is IComptroller {
    function getAccountLiquidity(address account)
        external
        view
        returns (uint256 err, uint256 liquidity, uint256 shortfall);
    function markets(address mToken) external view returns (bool isListed, uint256 collateralFactorMantissa);
    function getAssetsIn(address account) external view returns (address[] memory);
}

interface IERC20Full is IERC20 {
    function decimals() external view returns (uint8);
}

interface IMTokenFull is IMToken {
    function mint(uint256 mintAmount) external returns (uint256);
    function borrow(uint256 borrowAmount) external returns (uint256);
    function borrowBalanceStored(address account) external view returns (uint256);
    function exchangeRateStored() external view returns (uint256);
    function protocolSeizeShareMantissa() external view returns (uint256);
}

interface IComptrollerActions is IComptrollerFull {
    function enterMarkets(address[] calldata mTokens) external returns (uint256[] memory);
    function liquidateCalculateSeizeTokens(address mTokenBorrowed, address mTokenCollateral, uint256 actualRepayAmount)
        external
        view
        returns (uint256 err, uint256 seizeTokens);
}

interface IPriceOracle {
    function getUnderlyingPrice(address mToken) external view returns (uint256);
}

interface IWETH {
    function deposit() external payable;
}

/// Wrapper OEV palsu untuk fork test: meniru perilaku ChainlinkOEVWrapper
/// (tarik repay dari liquidator, liquidateBorrow, teruskan sitaan) tanpa
/// mekanisme auction. Di-etch ke alamat wrapper asli.
contract FakeOevWrapper {
    function updatePriceEarlyAndLiquidate(
        address borrower,
        uint256 repayAmount,
        address mTokenCollateral,
        address mTokenLoan
    ) external {
        IERC20 loan = IERC20(IMToken(mTokenLoan).underlying());
        loan.transferFrom(msg.sender, address(this), repayAmount);
        loan.approve(mTokenLoan, repayAmount);
        require(
            IMToken(mTokenLoan).liquidateBorrow(borrower, repayAmount, mTokenCollateral) == 0,
            "fake wrapper: liquidate gagal"
        );
        uint256 seized = IMToken(mTokenCollateral).balanceOf(address(this));
        IERC20(address(mTokenCollateral)).transfer(msg.sender, seized);
    }
}

contract OevLiquidatorTest is Test {
    // alamat diverifikasi dari Moonwell docs + on-chain
    address constant COMPTROLLER = 0xfBb21d0380beE3312B33c4353c8936a0F13EF26C;
    address constant ORACLE = 0xEC942bE8A8114bFD0396A5052c36027f2cA6a9d0;
    address constant MORPHO = 0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb;

    address constant M_WETH = 0x628ff693426583D9a7FB391E54366292F509D457;
    address constant WETH = 0x4200000000000000000000000000000000000006;

    OevLiquidator executor;
    address owner = address(this);

    function setUp() public {
        vm.createSelectFork(vm.envOr("BASE_RPC_URL", string("https://mainnet.base.org")));
        executor = new OevLiquidator();
    }

    function testConstants() public view {
        assertEq(address(executor.morpho()), MORPHO);
        assertEq(address(executor.comptroller()), COMPTROLLER);
        assertEq(executor.owner(), owner);
    }

    /// Wrapper untuk WETH di oracle harus mengarah ke ChainlinkOEVWrapper resmi.
    function testWrapperResolution() public view {
        IOracle oracle = IOracle(ORACLE);
        address wrapper = oracle.getFeed("WETH");
        assertTrue(wrapper != address(0), "wrapper WETH harus terdaftar");
    }

    /// Simulasi jalur OEV penuh memerlukan borrower underwater pada state fork.
    /// Tanpa data historis, kita pastikan eth_call revert terkontrol.
    /// Wrapper mungkin revert duluan karena flashloan tidak bisa dikembalikan
    /// (loan token habis dipakai repay) — keduanya harus revert.
    function testOevRevertsOnHealthyAccount() public {
        LiquidationJob memory job = LiquidationJob({
            mode: Mode.Oev,
            loanToken: IERC20(WETH),
            swapTarget: address(0),
            swapData: "",
            mTokenLoan: IMToken(M_WETH),
            mTokenCollateral: IMToken(M_WETH),
            borrower: owner,
            repayAmount: 0.001 ether,
            minProfit: 0,
            minLoanOut: 0
        });

        vm.expectRevert();
        executor.execute(job);
    }

    function testClassicRevertsOnHealthyAccount() public {
        LiquidationJob memory job = LiquidationJob({
            mode: Mode.Classic,
            loanToken: IERC20(WETH),
            swapTarget: address(0),
            swapData: "",
            mTokenLoan: IMToken(M_WETH),
            mTokenCollateral: IMToken(M_WETH),
            borrower: owner,
            repayAmount: 0.001 ether,
            minProfit: 0,
            minLoanOut: 0
        });

        vm.expectRevert();
        executor.execute(job);
    }

    function testOnlyOwnerCanExecute() public {
        LiquidationJob memory job = LiquidationJob({
            mode: Mode.Classic,
            loanToken: IERC20(WETH),
            swapTarget: address(0),
            swapData: "",
            mTokenLoan: IMToken(M_WETH),
            mTokenCollateral: IMToken(M_WETH),
            borrower: owner,
            repayAmount: 1,
            minProfit: 0,
            minLoanOut: 0
        });

        vm.prank(address(0xdead));
        vm.expectRevert("not owner");
        executor.execute(job);
    }

    function testOnlyOwnerCanSweep() public {
        vm.prank(address(0xdead));
        vm.expectRevert("not owner");
        executor.sweep(WETH, 1);
    }

    // --- validasi venue swap ---

    address constant AERODROME_ROUTER = 0xcF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43;
    address constant AERODROME_FACTORY = 0x420DD381b31aEf6683db6B902084cB0FFECe40Da;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;

    struct Route {
        address from;
        address to;
        bool stable;
        address factory;
    }

    /// Pastikan alamat router/factory Aerodrome valid dan calldata
    /// swapExactTokensForTokens bekerja persis seperti yang dibangun bot.
    function testAerodromeSwapWorks() public {
        uint256 amountIn = 1000e6; // 1.000 USDC
        deal(USDC, address(executor), amountIn);

        Route[] memory routes = new Route[](1);
        routes[0] = Route({from: USDC, to: WETH, stable: false, factory: AERODROME_FACTORY});

        // executor (owner) harus approve router -> calldata berjalan
        vm.prank(address(executor));
        (bool ok,) = USDC.call(
            abi.encodeWithSelector(IERC20.approve.selector, AERODROME_ROUTER, amountIn)
        );
        require(ok, "approve gagal");

        bytes memory swapData = abi.encodeWithSelector(
            bytes4(keccak256("swapExactTokensForTokens(uint256,uint256,(address,address,bool,address)[],address,uint256)")),
            amountIn,
            1,
            routes,
            address(executor),
            block.timestamp + 600
        );

        uint256 wethBefore = IERC20(WETH).balanceOf(address(executor));
        vm.prank(address(executor));
        (bool swapOk,) = AERODROME_ROUTER.call(swapData);
        require(swapOk, "swap revert: cek alamat router/factory");
        assertGt(IERC20(WETH).balanceOf(address(executor)), wethBefore, "WETH harus bertambah");
    }

    /// Temuan audit #1: siapa pun dapat mencuri reserve via callback
    /// tanpa flag yang menandai flashloan diforecksikan execute().
    function testCallbackRejectsForgedFlashloan() public {
        LiquidationJob memory job;
        job.mode = Mode.Oev;
        job.loanToken = IERC20(WETH);
        job.swapTarget = address(0);
        job.swapData = "";
        job.mTokenLoan = IMToken(M_WETH);
        job.mTokenCollateral = IMToken(M_WETH);
        job.borrower = address(0x1234);
        job.repayAmount = 1;
        job.minProfit = 0;
        job.minLoanOut = 0;

        // langsung panggil callback dari morpho — harus revert karena flag tidak diset
        vm.prank(MORPHO);
        vm.expectRevert("unknown flashloan");
        executor.onMorphoFlashLoan(1, abi.encode(job));
        assertEq(executor.expectedCallHash(), bytes32(0));
    }

    /// Flag diset hanya oleh execute() (onlyOwner), jadi forged call dari
    /// attacker tidak bisa memalsukan.
    function testOnlyMorphoCanCallCallback() public {
        // attacker memanggil onMorphoFlashLoan; revert karena bukan morpho
        LiquidationJob memory job;
        job.mode = Mode.Oev;
        job.loanToken = IERC20(WETH);
        job.mTokenLoan = IMToken(M_WETH);
        job.mTokenCollateral = IMToken(M_WETH);
        job.repayAmount = 1;

        vm.expectRevert("bad caller");
        executor.onMorphoFlashLoan(1, abi.encode(job));
    }

    // --- end-to-end happy path (fork Base) ---

    address constant M_USDC = 0xEdc817A28E8B93B03976FBd4a3dDBc9f7D176c22;

    /// Buat borrower nyata di fork: deposit 10 WETH, borrow 65% dari batas,
    /// lalu mock harga WETH jatuh 40% sehingga masuk shortfall.
    function _createUnderwaterBorrower() internal returns (address borrower) {
        borrower = makeAddr("borrower");
        IPriceOracle oracle = IPriceOracle(ORACLE);
        uint256 wethPrice = oracle.getUnderlyingPrice(M_WETH);
        (, uint256 cf) = IComptrollerFull(COMPTROLLER).markets(M_WETH);

        vm.deal(borrower, 10 ether);
        vm.startPrank(borrower);
        IWETH(WETH).deposit{value: 10 ether}();
        IERC20(WETH).approve(M_WETH, type(uint256).max);
        require(IMTokenFull(M_WETH).mint(10 ether) == 0, "mint gagal");
        address[] memory mkts = new address[](1);
        mkts[0] = M_WETH;
        IComptrollerActions(COMPTROLLER).enterMarkets(mkts);
        uint256 maxBorrowUsd = (10 ether * wethPrice / 1e18) * cf / 1e18;
        uint256 borrowUsd = maxBorrowUsd * 65 / 100;
        uint256 usdcPrice = oracle.getUnderlyingPrice(M_USDC);
        uint256 borrowAmt = borrowUsd * 1e18 / usdcPrice;
        require(IMTokenFull(M_USDC).borrow(borrowAmt) == 0, "borrow gagal");
        vm.stopPrank();

        vm.mockCall(
            ORACLE,
            abi.encodeWithSelector(IPriceOracle.getUnderlyingPrice.selector, M_WETH),
            abi.encode(wethPrice * 60 / 100)
        );
        (,, uint256 shortfall) = IComptrollerFull(COMPTROLLER).getAccountLiquidity(borrower);
        assertGt(shortfall, 0, "borrower harus underwater");
    }

    /// Bangun job realistis: repay = close factor, swap WETH -> USDC via
    /// Aerodrome dengan amountIn 99% dari estimasi sitaan (buffer pembulatan).
    function _buildJob(Mode mode, address borrower) internal view returns (LiquidationJob memory) {
        uint256 borrowBal = IMTokenFull(M_USDC).borrowBalanceStored(borrower);
        uint256 repay = borrowBal / 2; // close factor 50%
        (uint256 err, uint256 seizeTokens) = IComptrollerActions(COMPTROLLER)
            .liquidateCalculateSeizeTokens(M_USDC, M_WETH, repay);
        require(err == 0, "seize calc gagal");
        uint256 rate = IMTokenFull(M_WETH).exchangeRateStored();
        // liquidateCalculateSeizeTokens mengembalikan sitaan BRUTO; liquidator
        // hanya menerima (1 - protocolSeizeShare). Kurangi + buffer pembulatan.
        uint256 seizeShare = IMTokenFull(M_WETH).protocolSeizeShareMantissa();
        uint256 amountIn = seizeTokens * rate / 1e18 * (1e18 - seizeShare) / 1e18 * 99 / 100;

        Route[] memory routes = new Route[](1);
        routes[0] = Route({from: WETH, to: USDC, stable: false, factory: AERODROME_FACTORY});
        bytes memory swapData = abi.encodeWithSelector(
            bytes4(keccak256("swapExactTokensForTokens(uint256,uint256,(address,address,bool,address)[],address,uint256)")),
            amountIn,
            repay, // amountOutMin: minimal menutup flashloan
            routes,
            address(executor),
            block.timestamp + 600
        );

        return LiquidationJob({
            mode: mode,
            loanToken: IERC20(USDC),
            swapTarget: AERODROME_ROUTER,
            swapData: swapData,
            mTokenLoan: IMToken(M_USDC),
            mTokenCollateral: IMToken(M_WETH),
            borrower: borrower,
            repayAmount: repay,
            minProfit: 1,
            minLoanOut: 0
        });
    }

    /// Jalur B penuh: flashloan USDC -> liquidateBorrow -> redeem mWETH ->
    /// swap WETH->USDC di Aerodrome -> repay flashloan -> profit USDC > 0.
    function testClassicLiquidationEndToEnd() public {
        address borrower = _createUnderwaterBorrower();
        LiquidationJob memory job = _buildJob(Mode.Classic, borrower);

        uint256 usdcBefore = IERC20(USDC).balanceOf(address(executor));
        executor.execute(job);

        assertGt(
            IERC20(USDC).balanceOf(address(executor)),
            usdcBefore,
            "profit USDC harus > 0"
        );
        assertEq(executor.expectedCallHash(), bytes32(0), "call hash harus di-clear");
        assertEq(IERC20(USDC).allowance(address(executor), MORPHO), 0, "allowance morpho harus 0");
    }

    /// Jalur A penuh: wrapper OEV (di-etch FakeOevWrapper) mengarah ke
    /// liquidateBorrow; alur sisanya identik dengan jalur B.
    function testOevLiquidationEndToEnd() public {
        address borrower = _createUnderwaterBorrower();
        address wrapper = IOracle(ORACLE).getFeed("WETH");
        FakeOevWrapper fake = new FakeOevWrapper();
        vm.etch(wrapper, address(fake).code);

        LiquidationJob memory job = _buildJob(Mode.Oev, borrower);

        uint256 usdcBefore = IERC20(USDC).balanceOf(address(executor));
        executor.execute(job);

        assertGt(
            IERC20(USDC).balanceOf(address(executor)),
            usdcBefore,
            "profit USDC harus > 0"
        );
        assertEq(executor.expectedCallHash(), bytes32(0), "call hash harus di-clear");
        assertEq(IERC20(USDC).allowance(address(executor), wrapper), 0, "allowance wrapper harus 0");
    }

    /// Pertahanan: bila owner keliru mengisi swapData dengan amountIn lebih
    /// besar dari saldo kolateral aktual, transaksi harus revert — tidak ada
    /// dana yang hilang diam-diam.
    function testSwapAmountInExceedsBalanceReverts() public {
        address borrower = _createUnderwaterBorrower();
        LiquidationJob memory job = _buildJob(Mode.Classic, borrower);

        // rusak amountIn di swapData: decode payload (tanpa selector 4B),
        // gandakan, encode ulang dengan selector yang sama.
        bytes memory data = job.swapData;
        bytes4 selector;
        assembly { selector := mload(add(data, 32)) }
        bytes memory payload = new bytes(job.swapData.length - 4);
        for (uint256 i = 0; i < payload.length; i++) {
            payload[i] = job.swapData[i + 4];
        }
        (uint256 amountIn,, Route[] memory routes, address to, uint256 deadline) = abi.decode(
            payload,
            (uint256, uint256, Route[], address, uint256)
        );
        job.swapData = abi.encodeWithSelector(
            selector, amountIn * 2, 1, routes, to, deadline
        );

        vm.expectRevert();
        executor.execute(job);
    }

    /// Pertahanan: profit di bawah minProfit harus revert NotProfitable.
    function testRevertsWhenProfitBelowMin() public {
        address borrower = _createUnderwaterBorrower();
        LiquidationJob memory job = _buildJob(Mode.Classic, borrower);
        job.minProfit = type(uint256).max / 2; // mustahil tercapai

        vm.expectRevert();
        executor.execute(job);
    }

    /// Pertahanan: redeem mengirim ETH native untuk market WETH — kontrak
    /// harus menerima dan membungkusnya kembali (profit terukur di WETH).
    function testWethRedeemUnwrapsAndWrapsBack() public {
        address borrower = _createUnderwaterBorrower();
        LiquidationJob memory job = _buildJob(Mode.Classic, borrower);

        // ETH kontrak harus 0 sebelum & sesudah (semua di-wrap ke WETH)
        assertEq(address(executor).balance, 0, "ETH harus 0 sebelum");
        executor.execute(job);
        assertEq(address(executor).balance, 0, "ETH harus 0 sesudah (semua di-wrap)");
    }
}
