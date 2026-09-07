//! Application-level services for managed Exo Code agents.

mod recipe;

pub use recipe::{
    CreateSandboxFromRecipeRequest, RecipeService, SandboxRecipe, SandboxRecipeStep, SecretResolver,
};
