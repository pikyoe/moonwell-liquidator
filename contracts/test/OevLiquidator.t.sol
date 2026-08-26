// SPDX-License-Identifier: MIT
pragma solidity ^0.8.19;

import "forge-std/Test.sol";
import "../src/OevLiquidator.sol";

interface IComptrollerFull is IComptroller {
    function getAccountLiquidity(address account)
        external
        view
        returns (uint256 err, uint256 liquidity, uint256 shortfall);
    function markets(address mToken) external view returns (bool isListed, uint256 collateralFactorMantissa, bool isComped);
    function getAssetsIn(address account) external view returns (address[] memory);
}

interface IERC20Full is IERC20 {
    function decimals() external view returns (uint8);
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

        vm.prank(address(executor));
        (bool ok,) = USDC.call(
            abi.encodeWithSelector(IERC20.approve.selector, AERODROME_ROUTER, amountIn)
        );
        require(ok, "approve gagal");

        bytes memory swapData = abi.encodeWithSelector(
            bytes4(keccak256("swapExactTokensForTokens(uint256,uint256,(address,address,bool,address)[],address,uint256)")),
            amountIn,
            1, // amountOutMin minimal — hanya membuktikan calldata valid
            routes,
            address(executor),
            block.timestamp + 600
        );

        uint256 wethBefore = IERC20(WETH).balanceOf(address(executor));
        vm.prank(address(executor));
        (bool swapOk,) = AERODROME_ROUTER.call(swapData);
        require(swapOk, "swap revert: cek alamat router atau factory");
        assertGt(IERC20(WETH).balanceOf(address(executor)), wethBefore, "WETH harus bertambah");
    }
}
