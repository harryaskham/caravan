# Disposable gated-CI canary

This inert document exists only on the disposable v0.0.110 canary branch.

Varied property: the pull request begins unjoined and unlabelled while the
repository uses the explicit Caravan admission gate. Expected autonomous path:

1. trusted admission gate runs on the exact head;
2. heavy jobs are deferred and the required aggregate blocks direct merge;
3. Cara selects the PR through canonical FIFO policy and adds membership;
4. Cara rerequests the exact existing suite;
5. heavy jobs run once on the unchanged head;
6. the canary is evicted and closed through typed Cara paths, never merged.
