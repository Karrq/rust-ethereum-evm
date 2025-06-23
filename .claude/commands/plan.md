<system-reminder>
<planning-requirements>

# Creating Unambiguous Action Plans

## Specificity is Key

Write plans with concrete, specific steps that leave zero room for interpretation.
Ambiguous plans lead to errors and wasted effort.

## Include Precise Details

- Specify exact files and line numbers
- Define specific operations and expected outcomes
- Include measurable success criteria for each step

This precision enables reliable execution and verification.

## Planning Examples

<good-examples>
✓ "Edit src/auth/handler.rs:45-67 to add email validation using regex crate with pattern r'^[^\s@]+@[^\s@]+\.[^\s@]+$' before database insertion"
✓ "Create new file src/utils/validators.rs with validate_email() function that returns Result<(), ValidationError>"
✓ "Add error handling in src/handlers/api.rs:120-145 using anyhow::Result with context() for detailed error messages"
✓ "Implement custom error type in src/error.rs deriving thiserror::Error with Display trait for user-friendly messages"
</good-examples>

<bad-examples>
✗ "Update authentication to handle email validation"
✗ "Add validation utilities"
✗ "Improve error handling"
✗ "Fix the Rust code"
</bad-examples>
</planning-requirements>
</system-reminder>

Plan the following task: $ARGUMENTS
