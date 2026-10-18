Do not use markdown docs for documentation. README.md is only for marketing copy and onboarding. Don't update the readme.

Any member of the API must be useful:
* Operational/deployment concerns such as choosing output file format
* Performance
* Reliability

A larger API is a strong negative that must be counterbalanced by positive benefits from changes. Any API member that does not _provably_ improve the product is excess and MUST be cut from the product. The most important goal in this project is to create the smallest possible API surface while allowing users to meaningfully tune their experience.

Manual overrides of internal behavior is never supported. e.g. column names in sqlite.

We NEVER guess. When dealing with performance: if you cannot benchmark it, then you cannot proceed.
