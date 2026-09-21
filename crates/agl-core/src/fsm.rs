pub trait Fsm {
    type State;
    type Input;
    type Output;
    type Error;

    fn transition(
        &self,
        state: &Self::State,
        input: Self::Input,
    ) -> Result<Transition<Self::State, Self::Output>, Self::Error>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Transition<S, O> {
    pub state: S,
    pub output: O,
}
