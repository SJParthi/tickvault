# =============================================================================
# State-address migrations for the 2026-08-08 multi-AZ refactor (Quote 13).
#
# THE INCIDENT THIS FIXES (Verified, live AWS + workflow run 31295255920):
#
# The multi-AZ change rewrote two resources from a single instance to a
# `for_each` over ["a","b","c"]:
#
#     aws_subnet.public                  ->  aws_subnet.public["a"|"b"|"c"]
#     aws_route_table_association.public ->  aws_route_table_association.public[...]
#
# Terraform does NOT infer that the un-keyed address and the "a" instance are
# the same object. Without a `moved` block it plans to CREATE a brand-new
# subnet while the original still exists, and AWS refuses the duplicate CIDR:
#
#     Error: creating EC2 Subnet: InvalidSubnet.Conflict:
#     The CIDR '10.42.1.0/24' conflicts with another subnet
#       with aws_subnet.public["a"], on main.tf line 94
#
# The apply died mid-flight on 2026-08-09 at 06:05 UTC, leaving prod in a
# half-applied state (all Verified live via describe-* on 2026-08-09):
#
#   * the instance was destroyed and never recreated (Reservations: [])
#   * subnets "b" (10.42.2.0/24) and "c" (10.42.3.0/24) WERE created
#   * subnet "a" (10.42.1.0/24, subnet-00c8d06903d1482ea) still exists and is
#     still the un-keyed state object -- which is precisely WHY the create
#     collided: had it already been destroyed, the CIDR would have been free
#   * every route-table association was destroyed and none recreated, so the
#     public route table (rtb-0cc7c5b31afc5dffb) holds the 0.0.0.0/0 -> IGW
#     route with ZERO subnets attached, and all three subnets fall back to the
#     VPC main table (rtb-0c30dd479e4418240), which has only the local route.
#     An instance launched into any of them today would have NO internet path
#     and therefore no SSM, no Dhan, no deploys.
#
# These blocks re-point the existing state objects at their new keyed
# addresses so the next apply ADOPTS what already exists instead of trying to
# duplicate it, and then creates the three missing associations.
#
# SAFETY: a `moved` block whose source address is absent from state is a
# no-op, so this is correct whether or not a given object survived the partial
# apply -- it cannot itself destroy or recreate anything.
#
# The stranded 30 GB root volume (vol-073ccaa417a0f344b, ap-south-1a,
# "available") is NOT touched by any of this: it is managed by no terraform
# resource, and `delete_on_termination = false` (main.tf) is what preserved it
# when the instance was destroyed. EBS is zone-locked, so it cannot follow the
# instance to another AZ -- restoring that data is a deliberate, separate
# snapshot-and-restore step, not a side effect of this apply.
#
# DO NOT DELETE these blocks until an apply has succeeded and the state has
# been confirmed to carry the keyed addresses.
# =============================================================================

moved {
  from = aws_subnet.public
  to   = aws_subnet.public["a"]
}

moved {
  from = aws_route_table_association.public
  to   = aws_route_table_association.public["a"]
}
