# pluribus

personal distributed ai assistant

## how it works

1. [unum](./crates/pluribus-unum/) is a single node in the agent cluster deployment
2. nodes discover each other using tailscale acl tags (tag:pluribus)
3. state is shared via [loro](https://loro.dev) crdts
4. that means every node can see all messages, enabling, i.e. tool call delegation

## TODO

see [here](./TODO.txt)
