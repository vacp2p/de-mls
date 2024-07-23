// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.24;

import { Ownable } from "Openzeppelin/access/Ownable.sol";
import { IScKeystore, UserInfo, KeyPackage } from "./IScKeystore.sol";

error UserAlreadyExists();
error MalformedKeyPackage();
error MalformedUserInfo();
error UserDoesNotExist();

contract ScKeystore is Ownable, IScKeystore {
    event UserAdded(address user, bytes signaturePubKey);
    event UserKeyPackageAdded(address indexed user, uint256 index);

    mapping(address user => UserInfo userInfo) private users;
    KeyPackage[] private keyPackages;

    constructor(address initialOwner) Ownable(initialOwner) { }

    function userExists(address user) public view returns (bool) {
        return users[user].signaturePubKey.length > 0;
    }

    function addUser(bytes calldata signaturePubKey, KeyPackage calldata keyPackage) external onlyOwner {
        if (signaturePubKey.length == 0) revert MalformedUserInfo();
        if (keyPackage.data.length == 0) revert MalformedKeyPackage();
        if (userExists(msg.sender)) revert UserAlreadyExists();

        keyPackages.push(keyPackage);
        uint256 keyPackageIndex = keyPackages.length - 1;

        users[msg.sender] = UserInfo(new uint256[](0), signaturePubKey);
        users[msg.sender].signaturePubKey = signaturePubKey;
        users[msg.sender].keyPackageIndices.push(keyPackageIndex);

        emit UserAdded(msg.sender, signaturePubKey);
    }

    function getUser(address user) external view returns (UserInfo memory) {
        return users[user];
    }

    function addKeyPackage(KeyPackage calldata keyPackage) external {
        if (keyPackage.data.length == 0) revert MalformedKeyPackage();
        if (!userExists(msg.sender)) revert UserDoesNotExist();

        keyPackages.push(keyPackage);
        uint256 keyPackageIndex = keyPackages.length - 1;
        users[msg.sender].keyPackageIndices.push(keyPackageIndex);

        emit UserKeyPackageAdded(msg.sender, keyPackageIndex);
    }

    function getAvailableKeyPackage(address user) external view returns (KeyPackage memory) {
        UserInfo memory userInfo = users[user];
        uint256 keyPackageIndex = userInfo.keyPackageIndices[userInfo.keyPackageIndices.length - 1];
        return keyPackages[keyPackageIndex];
    }

    function getAllKeyPackagesForUser(address user) external view returns (KeyPackage[] memory) {
        UserInfo memory userInfo = users[user];
        KeyPackage[] memory userKeyPackages = new KeyPackage[](userInfo.keyPackageIndices.length);
        for (uint256 i = 0; i < userInfo.keyPackageIndices.length; i++) {
            userKeyPackages[i] = keyPackages[userInfo.keyPackageIndices[i]];
        }
        return userKeyPackages;
    }
}
