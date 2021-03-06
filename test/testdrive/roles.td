# Copyright Materialize, Inc. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

$ set-sql-timeout duration=1s

# Verify initial roles.
> SELECT id, name FROM mz_roles
-1 mz_system
 1 materialize

# Verify that invalid options are rejected.
! CREATE ROLE foo
non-login users not yet supported
! CREATE ROLE foo LOGIN
non-superusers not yet supported
! CREATE ROLE foo LOGIN LOGIN SUPERUSER
conflicting or redundant options
! CREATE ROLE foo LOGIN NOLOGIN SUPERUSER
conflicting or redundant options

# Create roles using both syntaxes and verify their existence.
> CREATE ROLE rj LOGIN SUPERUSER
> CREATE USER fms SUPERUSER
> SELECT id, name FROM mz_roles
-1 mz_system
 1 materialize
 2 rj
 3 fms

# Dropping multiple roles should not have any effect if one of the role names
# is bad...
! DROP ROLE rj, fms, bad
unknown role 'bad'
> SELECT id, name FROM mz_roles
-1 mz_system
 1 materialize
 2 rj
 3 fms

# ...unless IF EXISTS is specified.
> DROP ROLE IF EXISTS rj, fms, bad
> SELECT id, name FROM mz_roles
-1 mz_system
 1 materialize

# Verify that the single name version of DROP ROLE works too.
> CREATE ROLE nlb LOGIN SUPERUSER
> SELECT id, name FROM mz_roles
-1 mz_system
 1 materialize
 2 nlb
> DROP ROLE nlb
> SELECT id, name FROM mz_roles
-1 mz_system
 1 materialize
> DROP ROLE IF EXISTS nlb

# No dropping the current role.
! DROP ROLE ${testdrive.materialized-user}
current user cannot be dropped

# No creating roles that already exist.
! CREATE ROLE materialize LOGIN SUPERUSER
role 'materialize' already exists

# No creating roles that look like system roles.
! CREATE ROLE mz_system LOGIN SUPERUSER
role name "mz_system" is reserved
! CREATE ROLE mz_foo LOGIN SUPERUSER
role name "mz_foo" is reserved
