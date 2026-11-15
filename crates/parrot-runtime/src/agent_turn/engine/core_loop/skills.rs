use super::*;

pub(super) struct RuntimeSkillProvider<'a> {
    pub(super) skills: &'a dyn TurnSkills,
    pub(super) cancellation: &'a dyn Cancellation,
}

impl parrot_core::SkillProvider for RuntimeSkillProvider<'_> {
    fn resolve_skill_references<'a>(
        &'a self,
        references: &'a [parrot_core::SkillReference],
    ) -> BoxFuture<'a, Result<Vec<Option<parrot_core::Skill>>, CoreError>> {
        adapt_operation(async move {
            self.skills
                .get_skills(references, self.cancellation)
                .await
                .map_err(TurnError::from)
        })
    }
    fn read_skill_content<'a>(
        &'a self,
        skill: &'a parrot_core::Skill,
    ) -> BoxFuture<'a, Result<parrot_core::SkillContent, CoreError>> {
        adapt_operation(async move {
            self.skills
                .read_skill_content(skill, self.cancellation)
                .await
                .map_err(TurnError::from)
        })
    }
}
