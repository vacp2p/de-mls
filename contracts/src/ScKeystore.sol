// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.24;

import {IScKeystore, UserInfo, KeyPackage} from "./IScKeystore.sol";

error UserAlreadyExists();
error MalformedKeyPackage();
error MalformedUserInfo();
error UserDoesNotExist();

contract ScKeystore is IScKeystore {
    mapping(address user => UserInfo userInfo) private users;

    function userExists(address user) public view returns (bool) {
        return users[user].signaturePubKey.length > 0;
    }

    function addUser(UserInfo calldata userInfo) external {
        if(userInfo.signaturePubKey.length == 0) revert MalformedUserInfo();
        if(userInfo.keyPackages.length != 1) revert MalformedUserInfo();
        if(userExists(msg.sender)) revert UserAlreadyExists();

        users[msg.sender] = userInfo;
    }

    function getUser(address user) external view returns (UserInfo memory) {
        return users[user];
    }

    function addKeyPackage(KeyPackage calldata keyPackage) external {
        if(keyPackage.data.length == 0) revert MalformedKeyPackage();
        if(!userExists(msg.sender)) revert UserDoesNotExist();

        users[msg.sender].keyPackages.push(keyPackage);
    }

    function getAvailableKeyPackage(address user) external view returns (KeyPackage memory) {
        return users[user].keyPackages[users[user].keyPackages.length - 1];
    }
}
