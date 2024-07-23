// SPDX-License-Identifier: UNLICENSED
pragma solidity >=0.8.19 <=0.9.0;

import { ScKeystore } from "../src/ScKeystore.sol";
import { BaseScript } from "./Base.s.sol";
import { DeploymentConfig } from "./DeploymentConfig.s.sol";

contract Deploy is BaseScript {
    function run(address initialOwner)
        public
        broadcast
        returns (ScKeystore scKeystore, DeploymentConfig deploymentConfig)
    {
        deploymentConfig = new DeploymentConfig(broadcaster);
        scKeystore = new ScKeystore(initialOwner);
    }
}
