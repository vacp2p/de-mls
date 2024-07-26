// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.24;

import { Ownable } from "Openzeppelin/access/Ownable.sol";
import { IScKeystore, UserInfo } from "./IScKeystore.sol";

error UserAlreadyExists();
error MalformedUserInfo();
error UserDoesNotExist();

contract ScKeystore is Ownable, IScKeystore {
    event UserAdded(address user, bytes signaturePubKey);

    mapping(address user => UserInfo userInfo) private users;

    constructor(address initialOwner) Ownable(initialOwner) { }

    function userExists(address user) public view returns (bool) {
        return users[user].signaturePubKey.length > 0;
    }

    function addUser(address user, bytes calldata signaturePubKey) external onlyOwner {
        if (signaturePubKey.length == 0) revert MalformedUserInfo();
        if (userExists(user)) revert UserAlreadyExists();

        users[user] = UserInfo(signaturePubKey);

        emit UserAdded(user, signaturePubKey);
    }

    function getUser(address user) external view returns (UserInfo memory) {
        return users[user];
    }
}
