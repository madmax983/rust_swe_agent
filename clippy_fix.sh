#!/bin/bash
sed -i 's/network_pos.unwrap()/network_pos.expect("network_pos is some")/g' src/env/docker.rs
sed -i 's/args.iter().position(|a| a == "--network").unwrap()/args.iter().position(|a| a == "--network").expect("network")/g' src/env/docker.rs
sed -i 's/args.iter().position(|a| a == "my-image").unwrap()/args.iter().position(|a| a == "my-image").expect("image")/g' src/env/docker.rs
