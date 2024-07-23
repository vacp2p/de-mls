// SPDX-License-Identifier: UNLICENSED
pragma solidity >=0.8.19 <0.9.0;

import { Test } from "forge-std/Test.sol";
import { Deploy } from "../script/Deploy.s.sol";
import { DeploymentConfig } from "../script/DeploymentConfig.s.sol";
import "forge-std/console.sol";
import "../src/ScKeystore.sol"; // solhint-disable-line

contract ScKeystoreTest is Test {
  ScKeystore internal s;
  DeploymentConfig internal deploymentConfig;
  address internal deployer;

  function setUp() public virtual {
    Deploy deployment = new Deploy();
    (s, deploymentConfig) = deployment.run(address(this));
  }

  function addUser() internal {
    KeyPackage memory keyPackage = KeyPackage({ data: new bytes[](1) });
    s.addUser("0x", keyPackage);
  }

  function test__owner() public view {
    assert(s.owner() == address(this));
  }

  function test__userExists__returnsFalse__whenUserDoesNotExist() public view {
    assert(!s.userExists(address(this)));
  }

  function test__addUser__reverts__whenUserInfoIsMalformed() public {
    vm.expectRevert(MalformedUserInfo.selector);
    s.addUser("", KeyPackage({ data: new bytes[](0) }));
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

  function test__addUser__reverts__whenSenderIsNotOwner() public {
    vm.prank(address(0));
    vm.expectRevert();
    addUser();
    vm.stopPrank();
  }

  function test__getUser__returnsUserInfo__whenUserExists() public {
    addUser();
    UserInfo memory userInfo = s.getUser(address(this));
    assert(userInfo.signaturePubKey.length == 2);
    assert(userInfo.keyPackageIndices.length == 1);
  }

  function test__getAllKeyPackagesForUser__returnsKeyPackages__whenUserExists() public {
    addUser();
    KeyPackage[] memory keyPackages = s.getAllKeyPackagesForUser(address(this));
    assert(keyPackages.length == 1);
  }
}
