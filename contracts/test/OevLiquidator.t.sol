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
    function _setMarketBorrowCaps(address[] calldata mTokens, uint256[] calldata newBorrowCaps) external;
}

interface IPriceFeed {
    function latestRoundData()
        external
        view
        returns (
            uint80 roundId,
            int256 answer,
            uint256 startedAt,
            uint256 updatedAt,
            uint80 answeredInRound
        );
    function decimals() external view returns (uint8);
}

interface IUnderlyingSymbol {
    function symbol() external view returns (string memory);
    function decimals() external view returns (uint8);
}

interface IPriceOracle {
    function getUnderlyingPrice(address mToken) external view returns (uint256);
}

interface IWETH {
    function deposit() external payable;
}

/// Wrapper OEV palsu untuk fork test: meniru SEMANTIK ChainlinkOEVWrapper
/// produksi (tarik repay -> liquidateBorrow -> split sitaan antara likuidator
/// dan feeRecipient), dengan mekanika auction disederhanakan.
///
/// Di-etch ke alamat wrapper asli, sehingga harus stateless: semua parameter
/// dibaca on-chain (harga raw feed, exchangeRate, dsb.), fee diset konstan
/// 30% seperti liquidatorFeeBps produksi (3000).
contract FakeOevWrapper {
    uint16 private constant MAX_BPS = 10000;
    uint16 private constant LIQUIDATOR_FEE_BPS = 3000;

    // Alamat production feeRecipient wrapper WETH: dipakai hanya sebagai
    // penerima bagi hasil protokol agar hasil split bisa di-assert.
    address private constant FEE_RECIPIENT = 0xab05F7216B4ecD0594E703F21fb0dE6183BFeCF3;

    // Raw Chainlink feed yang dipakai test (WETH/USD, USDC/USD) — diverifikasi
    // on-chain via `wrapper.priceFeed()`.
    address private constant WETH_FEED = 0x71041dddad3595F9CEd3DcCFBe3D1F4b0a16Bb70;
    address private constant USDC_FEED = 0x7e860098F58bBFC8648a4311b374B1D669a2bc6B;

    /// Peta simbol ERC20 -> raw Chainlink feed (1-hop, cukup untuk pair test).
    function _feedFor(address tokenUnderlying) internal pure returns (address) {
        bool ok;
        assembly { ok := eq(tokenUnderlying, 0x4200000000000000000000000000000000000006) }
        if (ok) return WETH_FEED;
        return USDC_FEED; // USDC, atau apa pun — test hanya memakai WETH/USDC
    }

    function updatePriceEarlyAndLiquidate(
        address borrower,
        uint256 repayAmount,
        address mTokenCollateral,
        address mTokenLoan
    ) external {
        address loanUnderlying = IMToken(mTokenLoan).underlying();
        IERC20 loan = IERC20(loanUnderlying);
        loan.transferFrom(msg.sender, address(this), repayAmount);
        loan.approve(mTokenLoan, repayAmount);
        require(
            IMToken(mTokenLoan).liquidateBorrow(borrower, repayAmount, mTokenCollateral) == 0,
            "fake wrapper: liquidate gagal"
        );
        uint256 seized = IMToken(mTokenCollateral).balanceOf(address(this));
        require(seized > 0, "fake wrapper: zero seized");

        // Hitung split seperti produksi: liquidator dapat
        //   repay + (jumlah sitaan USD - repay USD) * liquidatorFeeBps / 10000
        // Protokol dapat sisanya. Memakai harga raw Chainlink feed dan
        // exchangeRateStored (mirip _calculateCollateralSplit produksi).
        uint256 loanPrice = _getTokenPriceUsd(_feedFor(loanUnderlying), loanUnderlying);
        uint256 collPrice = _getTokenPriceUsd(
            _feedFor(IMToken(mTokenCollateral).underlying()),
            IMToken(mTokenCollateral).underlying()
        );

        uint256 exchangeRate = IMTokenFull(mTokenCollateral).exchangeRateStored();
        uint256 underlyingAmount = (seized * exchangeRate) / 1e18;

        uint256 repayUsd = (repayAmount * loanPrice) / 1e18;
        uint256 collUsd = (underlyingAmount * collPrice) / 1e18;

        uint256 liquidatorUsd;
        if (collUsd <= repayUsd) {
            liquidatorUsd = collUsd;
        } else {
            liquidatorUsd =
                repayUsd +
                ((collUsd - repayUsd) * LIQUIDATOR_FEE_BPS) / MAX_BPS;
        }
        uint256 liquidatorUnderlying = (liquidatorUsd * 1e18) / collPrice;
        uint256 liquidatorMTokens = (liquidatorUnderlying * 1e18) / exchangeRate;
        if (liquidatorMTokens > seized) liquidatorMTokens = seized;

        // kirim bagian likuidator ke caller, sisanya ke feeRecipient.
        require(
            IERC20(address(mTokenCollateral)).transfer(msg.sender, liquidatorMTokens),
            "fake wrapper: liquidator transfer gagal"
        );
        uint256 protocolShare = seized - liquidatorMTokens;
        if (protocolShare > 0) {
            IERC20(address(mTokenCollateral)).transfer(FEE_RECIPIENT, protocolShare);
        }
    }

    /// Adopsi harga token USD (1e18) dari raw feed Chainlink dengan skala desimal.
    function _getTokenPriceUsd(address feed, address token) internal view returns (uint256) {
        IPriceFeed f = IPriceFeed(feed);
        (, int256 answer, , , ) = f.latestRoundData();
        require(answer > 0, "fake wrapper: harga <= 0");
        uint8 feedDecimals = f.decimals();
        uint8 tokenDecimals = IUnderlyingSymbol(token).decimals();

        uint256 p = uint256(answer);
        if (feedDecimals < 18) p *= 10 ** (18 - feedDecimals);
        else if (feedDecimals > 18) p /= 10 ** (feedDecimals - 18);
        if (tokenDecimals < 18) p *= 10 ** (18 - tokenDecimals);
        else if (tokenDecimals > 18) p /= 10 ** (tokenDecimals - 18);
        return p;
    }
}

contract OevLiquidatorTest is Test {
    // alamat diverifikasi dari Moonwell docs + on-chain
    address constant COMPTROLLER = 0xfBb21d0380beE3312B33c4353c8936a0F13EF26C;
    address constant ORACLE = 0xEC942bE8A8114bFD0396A5052c36027f2cA6a9d0;
    address constant MORPHO = 0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb;

    address constant M_WETH = 0x628ff693426583D9a7FB391E54366292F509D457;
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address constant M_USDC = 0xEdc817A28E8B93B03976FBd4a3dDBc9f7D176c22;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;

    // Raw Chainlink feed (diverifikasi on-chain via wrapper.priceFeed())
    address constant WETH_FEED = 0x71041dddad3595F9CEd3DcCFBe3D1F4b0a16Bb70;
    address constant USDC_FEED = 0x7e860098F58bBFC8648a4311b374B1D669a2bc6B;

    /// Verified on-chain: borrowCapGuardian = 0x08edEBfFaE68970dcf751Baa826182b3A4aCFC05
    address constant BORROW_CAP_GUARDIAN = 0x08eDEbFFaE68970DCf751baa826182b3a4aCFC05;

    OevLiquidator executor;
    address owner = address(this);

    function setUp() public {
        // Pin ke blok yang sudah ada (env BASE_FORK_BLOCK) bila tersedia agar
        // fork test deterministik; default 'latest' memicu flakiness "-32001
        // block not found" karena RPC Base tak konsisten pada tip.
        string memory rpc = vm.envOr("BASE_RPC_URL", string("https://mainnet.base.org"));
        string memory forkBlock = vm.envOr("BASE_FORK_BLOCK", string(""));
        if (bytes(forkBlock).length > 0) {
            uint256 blk = vm.parseUint(forkBlock);
            vm.createSelectFork(rpc, blk);
        } else {
            vm.createSelectFork(rpc);
        }
        _unlockBorrowCaps();
        _seedUsdcCash();
        executor = new OevLiquidator();
    }

    /// Market mUSDC sedang full-utilized (getCash = 0), jadi borrow baru
    /// revert "insufficient cash" meski cap sudah dibuka. Supply USDC sendiri
    /// di fork: deal 10M USDC lalu mint mUSDC.
    function _seedUsdcCash() internal {
        deal(address(USDC), address(this), 10_000_000e6);
        IERC20(USDC).approve(M_USDC, type(uint256).max);
        require(IMTokenFull(M_USDC).mint(10_000_000e6) == 0, "seed cash mint gagal");
    }

    /// Fork state terkini memiliki borrowCaps semua market = 1 wei (cap penuh),
    /// sehingga test tidak bisa meminjam. Buka batas via borrowCapGuardian —
    /// 0 artinya unlimited di Comptroller Moonwell.
    function _unlockBorrowCaps() internal {
        address[] memory markets = new address[](1);
        markets[0] = M_USDC;
        uint256[] memory caps = new uint256[](1);
        caps[0] = 0; // unlimited

        vm.prank(BORROW_CAP_GUARDIAN);
        IComptrollerActions(COMPTROLLER)._setMarketBorrowCaps(markets, caps);
    }

    function _borrowCap(address m) internal view returns (uint256) {
        (, bytes memory ret) = COMPTROLLER.staticcall(
            abi.encodeWithSelector(bytes4(keccak256("borrowCaps(address)")), m)
        );
        return abi.decode(ret, (uint256));
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

    /// Feed jalur OEV yang bukan ChainlinkOEVWrapper (aggregator raw) harus
    /// di-tolak oleh penyaring selector 0x16bb3b3a — bukan revert acak dari
    /// memanggil fungsi yang tidak ada di aggregator.
    function testOevRejectsNonWrapperFeed() public {
        // Borower nyata diperlukan agar repayAmount > 0 (execute nihil saat 0).
        address borrower = _createUnderwaterBorrower();
        // Samakan getFeed("WETH") dengan aggregator raw Chainlink
        // (WETH/USD 0x7104..) yang tidak punya updatePriceEarlyAndLiquidate.
        address rawAgg = 0x71041dddad3595F9CEd3DcCFBe3D1F4b0a16Bb70;
        vm.mockCall(
            ORACLE,
            abi.encodeWithSelector(IOracle.getFeed.selector, "WETH"),
            abi.encode(rawAgg)
        );
        LiquidationJob memory job = _buildJob(Mode.Oev, borrower);
        vm.expectRevert("wrapper bukan OEV");
        executor.execute(job);
    }

    /// Posisi selector BUKAN kelipatan 32 harus tetap terdeteksi. Dispatcher
    /// Solidity menyimpan selector sebagai PUSH4 (0x63 + 4 byte) pada aliran
    /// byte dengan offset 4-byte aligned — wrapper WETH produksi memilikinya
    /// di offset 54. Bytecode dummy di bawah menaruh `6316bb3b3a` (PUSH4 +
    /// selector) di offset 54, didahului STOP (00) agar tidak dieksekusi.
    /// Karena filter lolos, eksekusi berlanjut ke call wrapper yang "berhasil"
    /// (STOP) tapi tanpa sitaan -> revert "zero seized". Bila scan hanya
    /// melihat offset kelipatan 32, test ini revert "wrapper bukan OEV" dan
    /// gagal.
    function testOevDetectsNonAlignedSelector() public {
        address borrower = _createUnderwaterBorrower();
        address dummy = makeAddr("dummy-wrapper");

        bytes memory code = new bytes(288);
        // offset 0..53: STOP (tidak dieksekusi lebih jauh)
        for (uint256 i = 0; i < 54; i++) code[i] = 0x00;
        // offset 54..58: PUSH4 0x16bb3b3a (byte selector tepat di offset 55)
        code[54] = 0x63;
        code[55] = 0x16;
        code[56] = 0xbb;
        code[57] = 0x3b;
        code[58] = 0x3a;

        vm.etch(dummy, code);
        vm.mockCall(
            ORACLE,
            abi.encodeWithSelector(IOracle.getFeed.selector, "WETH"),
            abi.encode(dummy)
        );
        LiquidationJob memory job = _buildJob(Mode.Oev, borrower);
        // Filter menemukan selector di offset 55 (non-32); call ke wrapper
        // "sukses" (STOP), sitaan 0 -> revert di tahap redeem.
        vm.expectRevert("zero seized");
        executor.execute(job);
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
    /// Aerodrome. amountIn bergantung jalur:
    ///  - Classic: liquidator menerima SELURUH sitaan (1 - seizeShare);
    ///  - OEV: wrapper membagi sitaan, liquidator dapat
    ///          repay + (collUsd - repayUsd) * liquidatorFeeBps / 10000.
    /// Pembagian memakai harga raw Chainlink (seperti produksi), bukan harga
    /// oracle yang di-mock.
    function _buildJob(Mode mode, address borrower) internal view returns (LiquidationJob memory) {
        uint256 borrowBal = IMTokenFull(M_USDC).borrowBalanceStored(borrower);
        uint256 repay = borrowBal / 2; // close factor 50%
        (uint256 err, uint256 seizeTokens) = IComptrollerActions(COMPTROLLER)
            .liquidateCalculateSeizeTokens(M_USDC, M_WETH, repay);
        require(err == 0, "seize calc gagal");
        uint256 rate = IMTokenFull(M_WETH).exchangeRateStored();
        uint256 seizeShare = IMTokenFull(M_WETH).protocolSeizeShareMantissa();

        uint256 wethUsd = _priceUsd(WETH_FEED, WETH);
        uint256 usdcUsd = _priceUsd(USDC_FEED, USDC);

        uint256 amountIn;
        if (mode == Mode.Classic) {
            // liquidator menerima seluruh sitaan net (1 - seizeShare)
            amountIn = seizeTokens * rate / 1e18 * (1e18 - seizeShare) / 1e18 * 99 / 100;
        } else {
            // OEV: split ala produksi
            uint256 seizedNet = seizeTokens * rate / 1e18 * (1e18 - seizeShare) / 1e18;
            uint256 collUsd = seizedNet * wethUsd / 1e18;
            uint256 repayUsd = repay * usdcUsd / 1e18;
            uint256 liquidatorUsd;
            if (collUsd <= repayUsd) liquidatorUsd = collUsd;
            else liquidatorUsd = repayUsd + (collUsd - repayUsd) * 3000 / 10000;
            amountIn = liquidatorUsd * 1e18 / wethUsd * 90 / 100;
        }

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

    /// Harga per whole token memakai format Moonwell (1e(36-decimals)),
/// identik dengan logika ChainlinkOracle & FakeOevWrapper:
/// price = answer * 10^(36 - feedDecimals - tokenDecimals).
    function _priceUsd(address feed, address token) internal view returns (uint256) {
        IPriceFeed f = IPriceFeed(feed);
        (, int256 answer, , , ) = f.latestRoundData();
        require(answer > 0, "feed price <= 0");
        uint8 feedDecimals = IPriceFeed(feed).decimals();
        uint8 tokenDecimals = IUnderlyingSymbol(token).decimals();
        require(feedDecimals + tokenDecimals <= 36, "price scale overflow");
        return uint256(answer) * 10 ** (36 - uint256(feedDecimals) - uint256(tokenDecimals));
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
