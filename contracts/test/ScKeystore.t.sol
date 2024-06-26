// SPDX-License-Identifier: UNLICENSED
pragma solidity >=0.8.19 <0.9.0;

import { Test } from "forge-std/Test.sol";
import { Deploy } from "../script/Deploy.s.sol";
import { DeploymentConfig } from "../script/DeploymentConfig.s.sol";
import "../src/ScKeystore.sol"; // solhint-disable-line

contract ScKeystoreTest is Test {
    ScKeystore internal s;
    DeploymentConfig internal deploymentConfig;
    address internal deployer;

    function setUp() public virtual {
        Deploy deployment = new Deploy();
        (s, deploymentConfig) = deployment.run();
    }

    function addUser() internal {
        KeyPackage[] memory keyPackages = new KeyPackage[](1);
        keyPackages[0] = KeyPackage({data: new bytes[](0)});
        UserInfo memory userInfo = UserInfo({
            signaturePubKey: "0x",
            keyPackages: keyPackages
        });
        s.addUser(userInfo);
    }

    function test__userExists__returnsFalse__whenUserDoesNotExist() public view {
        assert(!s.userExists(address(this)));
    }

    function test__addUser__reverts__whenUserInfoIsMalformed() public {
        UserInfo memory userInfo;
        vm.expectRevert(MalformedUserInfo.selector);
        s.addUser(userInfo);
    }

    function test__addUser__reverts__whenUserAlreadyExists() public {
        addUser();
        vm.expectRevert(UserAlreadyExists.selector);
        addUser();
    }

    function test__addUser__addsUser__whenUserInfoIsValid() public {
        addUser();
        assert(s.userExists(address(this)));
    }

    function test__getUser__returnsUserInfo__whenUserExists() public {
        addUser();
        UserInfo memory userInfo = s.getUser(address(this));
        assert(userInfo.signaturePubKey.length == 2);
        assert(userInfo.keyPackages.length == 1);
    }
}
